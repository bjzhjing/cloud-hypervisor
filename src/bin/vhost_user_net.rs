// Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

#[macro_use(crate_version, crate_authors)]
extern crate clap;
extern crate log;
extern crate net_util;
extern crate vhost_rs;
extern crate vhost_user_backend;
extern crate vm_virtio;

use clap::{App, Arg};
use epoll;
use libc::{self, EAGAIN, EFD_NONBLOCK};
use log::*;
use std::cmp;
use std::io::Read;
use std::io::{self, Write};
use std::mem;
use std::net::Ipv4Addr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::process;
use std::sync::{Arc, RwLock};
use std::vec::Vec;

use vhost_rs::vhost_user::message::*;
use vhost_rs::vhost_user::Error as VhostUserError;
use vhost_user_backend::{VhostUserBackend, VhostUserDaemon, Vring};

use net_gen;

use net_util::{Tap, TapError};
use virtio_bindings::bindings::virtio_net::*;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

/// The maximum buffer size when segmentation offload is enabled. This
/// includes the 12-byte virtio net header.
/// http://docs.oasis-open.org/virtio/virtio/v1.0/virtio-v1.0.html#x1-1740003
const MAX_BUFFER_SIZE: usize = 65562;
const QUEUE_SIZE: usize = 256;
const NUM_QUEUES: usize = 2;
const QUEUE_SIZES: &[usize] = &[QUEUE_SIZE; NUM_QUEUES];

// A frame is available for reading from the tap device to receive in the guest.
const RX_TAP_EVENT: u16 = 2;
// The guest has made a buffer available to receive a frame into.
const RX_QUEUE_EVENT: u16 = 0;
// The transmit queue has a frame that is ready to send from the guest.
const TX_QUEUE_EVENT: u16 = 1;
// The device has been dropped.
pub const KILL_EVENT: u16 = 3;
// Number of DeviceEventT events supported by this implementation.
pub const NET_EVENTS_COUNT: u16 = 4;

pub type VhostUserResult<T> = std::result::Result<T, VhostUserError>;
pub type Result<T> = std::result::Result<T, Error>;
pub type VhostUserBackendResult<T> = std::result::Result<T, std::io::Error>;

#[derive(Debug)]
pub enum Error {
    /// Failed to activate device.
    BadActivate,
    /// Failed to create kill eventfd
    CreateKillEventFd,
    /// Failed to add event.
    EpollCtl(io::Error),
    /// Fail to wait event.
    EpollWait(io::Error),
    /// Failed to create EventFd.
    EpollCreateFd,
    /// Failed to read Tap.
    FailedReadTap,
    /// Failed to signal used queue.
    FailedSignalingUsedQueue,
    /// Invalid vring address.
    InvalidVringAddr,
    /// No vring call fd to notify.
    NoVringCallFdNotify,
    /// No memory while handling tx.
    NoMemoryForTx,
    /// Failed to parse sock parameter.
    ParseSockParam,
    /// Failed to parse ip parameter.
    ParseIpParam,
    /// Failed to parse mask parameter.
    ParseMaskParam,
    /// Open tap device failed.
    TapOpen(TapError),
    /// Setting tap IP failed.
    TapSetIp(TapError),
    /// Setting tap netmask failed.
    TapSetNetmask(TapError),
    /// Setting tap interface offload flags failed.
    TapSetOffload(TapError),
    /// Setting vnet header size failed.
    TapSetVnetHdrSize(TapError),
    /// Enabling tap interface failed.
    TapEnable(TapError),
}

#[derive(Clone)]
pub struct TxVirtio {
    pub vring: Arc<RwLock<Vring>>,
    iovec: Vec<(GuestAddress, usize)>,
    frame_buf: [u8; MAX_BUFFER_SIZE],
}

impl TxVirtio {
    fn new(vring: Arc<RwLock<Vring>>) -> Self {
        let tx_queue_max_size = vring.read().unwrap().queue.get_max_size() as usize;
        TxVirtio {
            vring,
            iovec: Vec::with_capacity(tx_queue_max_size),
            frame_buf: [0u8; MAX_BUFFER_SIZE],
        }
    }
}

#[derive(Clone)]
pub struct RxVirtio {
    deferred_frame: bool,
    deferred_irqs: bool,
    pub vring: Arc<RwLock<Vring>>,
    bytes_read: usize,
    frame_buf: [u8; MAX_BUFFER_SIZE],
}

impl RxVirtio {
    fn new(vring: Arc<RwLock<Vring>>) -> Self {
        RxVirtio {
            deferred_frame: false,
            deferred_irqs: false,
            vring,
            bytes_read: 0,
            frame_buf: [0u8; MAX_BUFFER_SIZE],
        }
    }
}

fn vnet_hdr_len() -> usize {
    mem::size_of::<virtio_net_hdr_v1>()
}

#[derive(Clone)]
pub struct VhostUserNetBackend {
    pub mem: Option<GuestMemoryMmap>,
    pub kill_evt: EventFd,
    pub tap: Tap,
    pub rx: RxVirtio,
    pub tx: TxVirtio,
    pub rx_tap_listening: bool,
    pub epoll_fd: RawFd,
}

impl VhostUserNetBackend {
    /// Create a new virtio network device with the given TAP interface.
    pub fn new_with_tap(tap: Tap) -> Result<Self> {
        // Set offload flags to match the virtio features below.
        tap.set_offload(
            net_gen::TUN_F_CSUM | net_gen::TUN_F_UFO | net_gen::TUN_F_TSO4 | net_gen::TUN_F_TSO6,
        )
        .map_err(Error::TapSetOffload)?;

        let vnet_hdr_size = vnet_hdr_len() as i32;
        tap.set_vnet_hdr_size(vnet_hdr_size)
            .map_err(Error::TapSetVnetHdrSize)?;

        // Here is a temporary implementation to create Rx/Tx queue, multiple queue is not
        // supported currently, so just simply add two queues here.
        let vrings = vec![Arc::new(RwLock::new(Vring::new(QUEUE_SIZE as u16))); NUM_QUEUES];

        let rx = RxVirtio::new(vrings[0].clone());
        let tx = TxVirtio::new(vrings[1].clone());

        Ok(VhostUserNetBackend {
            mem: None,
            kill_evt: EventFd::new(EFD_NONBLOCK).map_err(|_| Error::CreateKillEventFd)?,
            tap,
            rx,
            tx,
            rx_tap_listening: false,
            epoll_fd: 0,
        })
    }

    /// Create a new virtio network device with the given IP address and
    /// netmask.
    pub fn new(ip_addr: Ipv4Addr, netmask: Ipv4Addr) -> Result<Self> {
        let tap = Tap::new().map_err(Error::TapOpen)?;
        tap.set_ip_addr(ip_addr).map_err(Error::TapSetIp)?;
        tap.set_netmask(netmask).map_err(Error::TapSetNetmask)?;
        tap.enable().map_err(Error::TapEnable)?;

        Self::new_with_tap(tap)
    }

    fn signal_used_queue(&self, vring: &Vring) -> Result<()> {
        if let Some(call) = &vring.call {
            call.write(1).map_err(|_| Error::FailedSignalingUsedQueue)?;
        }
        Ok(())
    }

    // Copies a single frame from `self.rx.frame_buf` into the guest. Returns true
    // if a buffer was used, and false if the frame must be deferred until a buffer
    // is made available by the driver.
    fn rx_single_frame(&mut self) -> bool {
        if let Some(mem) = &self.mem {
            let mut next_desc = self.rx.vring.write().unwrap().queue.iter(&mem).next();

            if next_desc.is_none() {
                println!("rx queue has no available descriptors!\n");
                // Queue has no available descriptors
                if self.rx_tap_listening {
                    self.unregister_tap_rx_listener().unwrap();
                    self.rx_tap_listening = false;
                }
                return false;
            }

            // We just checked that the head descriptor exists.
            let head_index = next_desc.as_ref().unwrap().index;
            let mut write_count = 0;

            // Copy from frame into buffer, which may span multiple descriptors.
            loop {
                match next_desc {
                    Some(desc) => {
                        if !desc.is_write_only() {
                            break;
                        }
                        let limit = cmp::min(write_count + desc.len as usize, self.rx.bytes_read);
                        let source_slice = &self.rx.frame_buf[write_count..limit];
                        let write_result = mem.write_slice(source_slice, desc.addr);

                        match write_result {
                            Ok(_) => {
                                write_count = limit;
                            }
                            Err(e) => {
                                error!("Failed to write slice: {:?}", e);
                                break;
                            }
                        };

                        if write_count >= self.rx.bytes_read {
                            break;
                        }
                        next_desc = desc.next_descriptor();
                    }
                    None => {
                        warn!("Receiving buffer is too small to hold frame of current size");
                        break;
                    }
                }
            }

            self.rx
                .vring
                .write()
                .unwrap()
                .queue
                .add_used(&mem, head_index, write_count as u32);

            // Mark that we have at least one pending packet and we need to interrupt the guest.
            self.rx.deferred_irqs = true;

            write_count >= self.rx.bytes_read
        } else {
            error!("No memory for single frame handling!\n");
            false
        }
    }

    fn process_rx(&mut self) -> Result<()> {
        // Read as many frames as possible.
        loop {
            match self.read_tap() {
                Ok(count) => {
                    self.rx.bytes_read = count;
                    if !self.rx_single_frame() {
                        println!("read_tap done, set rx_single_frame!\n");
                        self.rx.deferred_frame = true;
                        break;
                    }
                }
                Err(e) => {
                    // The tap device is non-blocking, so any error aside from EAGAIN is
                    // unexpected.
                    match e.raw_os_error() {
                        Some(err) if err == EAGAIN => (),
                        _ => {
                            error!("Failed to read tap: {:?}", e);
                            return Err(Error::FailedReadTap);
                        }
                    };
                    break;
                }
            }
        }
        if self.rx.deferred_irqs {
            self.rx.deferred_irqs = false;
            self.signal_used_queue(&self.rx.vring.write().unwrap())
        } else {
            Ok(())
        }
    }

    fn resume_rx(&mut self) -> Result<()> {
        if self.rx.deferred_frame {
            if self.rx_single_frame() {
                self.rx.deferred_frame = false;
                // process_rx() was interrupted possibly before consuming all
                // packets in the tap; try continuing now.
                self.process_rx()
            } else if self.rx.deferred_irqs {
                self.rx.deferred_irqs = false;
                self.signal_used_queue(&self.rx.vring.write().unwrap())
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    fn process_tx(&mut self) -> Result<()> {
        if let Some(mem) = &self.mem {
            println!("Entering process_tx for vhost-user-net!\n");
            let mut used_desc_heads = [(0, 0); QUEUE_SIZE];
            let mut used_count = 0;
            while let Some(avail_desc) = self.tx.vring.write().unwrap().queue.iter(&mem).next() {
                println!("avail_desc is not none in process_tx for vhost-user-net!\n");
                let head_index = avail_desc.index;
                let mut read_count = 0;
                let mut next_desc = Some(avail_desc);

                self.tx.iovec.clear();
                while let Some(desc) = next_desc {
                    if desc.is_write_only() {
                        println!("desc is write only!\n");
                        break;
                    }
                    self.tx.iovec.push((desc.addr, desc.len as usize));
                    read_count += desc.len as usize;
                    next_desc = desc.next_descriptor();
                }
                used_desc_heads[used_count] = (head_index, read_count);
                used_count += 1;
                read_count = 0;
                // Copy buffer from across multiple descriptors.
                // TODO(performance - Issue #420): change this to use `writev()` instead of `write()`
                // and get rid of the intermediate buffer.
                for (desc_addr, desc_len) in self.tx.iovec.drain(..) {
                    let limit = cmp::min((read_count + desc_len) as usize, self.tx.frame_buf.len());

                    let read_result = mem.read_slice(
                        &mut self.tx.frame_buf[read_count..limit as usize],
                        desc_addr,
                    );
                    match read_result {
                        Ok(_) => {
                            println!(
                                "process_tx read_result is good with read_count: {:?}\n",
                                read_count
                            );
                            // Increment by number of bytes actually read
                            read_count += limit - read_count;
                        }
                        Err(e) => {
                            println!("Failed to read slice: {:?}", e);
                            break;
                        }
                    }
                }

                let write_result = self.tap.write(&self.tx.frame_buf[..read_count as usize]);
                match write_result {
                    Ok(_) => {}
                    Err(e) => {
                        println!("net: tx: error failed to write to tap: {}", e);
                    }
                };

                //self.tx
                //    .vring
                //    .write()
                //    .unwrap()
                //    .queue
                //    .add_used(&mem, head_index, 0);
            }

            for &(desc_index, _) in &used_desc_heads[..used_count] {
                self.tx
                    .vring
                    .write()
                    .unwrap()
                    .queue
                    .add_used(&mem, desc_index, 0);
            }
            self.signal_used_queue(&self.tx.vring.write().unwrap())
                .unwrap();
        } else {
            error!("No memory for vhost-user-net backend tx handling!\n");
            return Err(Error::NoMemoryForTx);
        }
        Ok(())
    }

    fn read_tap(&mut self) -> io::Result<usize> {
        self.tap.read(&mut self.rx.frame_buf)
    }

    fn register_tap_rx_listener(&self) -> std::result::Result<(), std::io::Error> {
        epoll::ctl(
            self.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.tap.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(RX_TAP_EVENT)),
        )?;
        Ok(())
    }

    fn unregister_tap_rx_listener(&self) -> std::result::Result<(), std::io::Error> {
        epoll::ctl(
            self.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_DEL,
            self.tap.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(RX_TAP_EVENT)),
        )?;
        Ok(())
    }
}

impl VhostUserBackend for VhostUserNetBackend {
    fn num_queues(&self) -> usize {
        NUM_QUEUES
    }

    fn max_queue_size(&self) -> &[usize] {
        QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        1 << VIRTIO_NET_F_GUEST_CSUM
            | 1 << VIRTIO_NET_F_CSUM
            | 1 << VIRTIO_NET_F_GUEST_TSO4
            | 1 << VIRTIO_NET_F_GUEST_UFO
            | 1 << VIRTIO_NET_F_HOST_TSO4
            | 1 << VIRTIO_NET_F_HOST_UFO
            | 1 << VIRTIO_F_VERSION_1
       //     | 1 << VIRTIO_RING_F_EVENT_IDX
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        evset: epoll::Events,
    ) -> VhostUserBackendResult<bool> {
        if evset != epoll::Events::EPOLLIN {
            println!("Invalid events operation!\n");
            return Ok(false);
        }
        match device_event {
            RX_QUEUE_EVENT => {
                println!("RX_QUEUE_EVENT received");
                self.resume_rx().unwrap();
                if !self.rx_tap_listening {
                    self.register_tap_rx_listener()?;
                    self.rx_tap_listening = true;
                }
            }
            TX_QUEUE_EVENT => {
                println!("TX_QUEUE_EVENT received!\n");
                self.process_tx().unwrap();
            }
            RX_TAP_EVENT => {
                println!("RX_TAP_EVENT received");
                if self.rx.deferred_frame
                // Process a deferred frame first if available. Don't read from tap again
                // until we manage to receive this deferred frame.
                {
                    if self.rx_single_frame() {
                        self.rx.deferred_frame = false;
                        self.process_rx().unwrap();
                    } else if self.rx.deferred_irqs {
                        self.rx.deferred_irqs = false;
                        self.signal_used_queue(&self.rx.vring.write().unwrap())
                            .unwrap();
                    }
                } else {
                    self.process_rx().unwrap();
                }
            }
            KILL_EVENT => {
                println!("KILL_EVENT received, stopping epoll loop");
            }
            _ => {
                println!("Unknown event for vhost-user-net-backend");
            }
        }
        Ok(true)
    }

    fn update_rx_vring(&mut self, vring: Arc<RwLock<Vring>>) {
        self.rx.vring = vring;
    }

    fn update_tx_vring(&mut self, vring: Arc<RwLock<Vring>>) {
        self.tx.vring = vring;
    }

    fn update_memory(&mut self, mem: Option<GuestMemoryMmap>) {
        self.mem = mem;
        if self.mem.is_some() {
            println!("memory is not none after update!\n");
        }
    }

    fn update_epollfd(&mut self, epoll_fd: RawFd) {
        self.epoll_fd = epoll_fd;
    }
}

pub struct VhostUserNetBackendConfig<'a> {
    pub ip: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub sock: &'a str,
}

impl<'a> VhostUserNetBackendConfig<'a> {
    pub fn parse(backend: &'a str) -> Result<Self> {
        let params_list: Vec<&str> = backend.split(',').collect();

        let mut ip_str: &str = "";
        let mut mask_str: &str = "";
        let mut sock: &str = "";

        for param in params_list.iter() {
            if param.starts_with("ip=") {
                ip_str = &param[3..];
            } else if param.starts_with("mask=") {
                mask_str = &param[5..];
            } else if param.starts_with("sock=") {
                sock = &param[5..];
            }
        }

        let mut ip: Ipv4Addr = Ipv4Addr::new(192, 168, 100, 1);
        let mut mask: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);

        if sock.is_empty() {
            return Err(Error::ParseSockParam)?;
        }
        if !ip_str.is_empty() {
            ip = ip_str.parse().map_err(|_| Error::ParseIpParam)?;
        }
        if !mask_str.is_empty() {
            mask = mask_str.parse().map_err(|_| Error::ParseMaskParam)?;
        }

        Ok(VhostUserNetBackendConfig { ip, mask, sock })
    }
}

fn main() {
    let cmd_arguments = App::new("vhost-user-net backend")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Launch a vhost-user-net backend.")
        .arg(
            Arg::with_name("backend")
                .long("backend")
                .help(
                    "Backend parameters \"ip=<ip_addr>,\
                     mask=<net_mask>,sock=<socket_path>\"",
                )
                .takes_value(true)
                .min_values(1),
        )
        .get_matches();

    let vhost_user_net_backend = cmd_arguments.value_of("backend").unwrap();

    let backend_config = match VhostUserNetBackendConfig::parse(vhost_user_net_backend) {
        Ok(config) => config,
        Err(e) => {
            println!("Failed parsing parameters {:?}", e);
            process::exit(1);
        }
    };

    let net_backend = VhostUserNetBackend::new(backend_config.ip, backend_config.mask).unwrap();
    println!("net_backend is created!\n");

    let name = "vhost-user-net-backend";
    let mut net_daemon = VhostUserDaemon::new(
        name.to_string(),
        backend_config.sock.to_string(),
        net_backend.clone(),
    )
    .unwrap();
    println!("net_daemon is created!\n");

    if let Err(e) = net_daemon.register_listener(
        net_backend.tap.as_raw_fd(),
        epoll::Events::EPOLLIN,
        u64::from(RX_TAP_EVENT),
    ) {
        println!("failed to regsiter listener for tap with error: {:?}\n", e);
        process::exit(1);
    }

    if net_daemon
        .register_listener(
            net_backend.kill_evt.as_raw_fd(),
            epoll::Events::EPOLLIN,
            u64::from(KILL_EVENT),
        )
        .is_err()
    {
        println!("failed to regsiter listener for kill event\n");
    }

    if let Err(e) = net_daemon.start() {
        println!(
            "failed to start daemon for vhost-user-net with error: {:?}\n",
            e
        );
        process::exit(1);
    }

    net_daemon.wait().unwrap();

    loop {
        std::thread::sleep(std::time::Duration::new(10, 0));
    }
}
