    //! Contains utilites for working with virtual (TAP) network interfaces.

    use priv_prelude::*;
    use sys;
    use iface::build::{IfaceBuilder, build};

    /// This object can be used to set the configuration options for a `EtherIface` before creating the
    /// `EtherIface`
    /// using `build`.
    #[derive(Debug)]
    pub struct EtherIfaceBuilder {
        builder: IfaceBuilder,
    }

    impl Default for EtherIfaceBuilder {
        fn default() -> EtherIfaceBuilder {
            EtherIfaceBuilder {
                builder: IfaceBuilder {
                    name: String::from("netsim"),
                    address: ipv4!("0.0.0.0"),
                    netmask: ipv4!("0.0.0.0"),
                    routes: Vec::new(),
                },
            }
        }
    }

    impl EtherIfaceBuilder {
        /// Start building a new `EtherIface` with the default configuration options.
        pub fn new() -> EtherIfaceBuilder {
            Default::default()
        }

        /// Set the interface name.
        pub fn name<S: Into<String>>(mut self, name: S) -> Self {
            self.builder.name = name.into();
            self
        }

        /// Set the interface address.
        pub fn address(mut self, address: Ipv4Addr) -> Self {
            self.builder.address = address;
            self
        }

        /// Set the interface netmask.
        pub fn netmask(mut self, netmask: Ipv4Addr) -> Self {
            self.builder.netmask = netmask;
            self
        }

        /// Add a route to the set of routes that will be created and directed through this interface.
        pub fn route(mut self, route: RouteV4) -> Self {
            self.builder.routes.push(route);
            self
        }

        /// Consume this `EtherIfaceBuilder` and build a `UnboundEtherIface`. This creates the TAP device but does not
        /// bind it to a tokio event loop. This is useful if the event loop lives in a different thread
        /// to where you need to create the device. You can send a `UnboundEtherIface` to another thread then
        /// `bind` it to create your `EtherIface`.
        pub fn build_unbound(self) -> Result<UnboundEtherIface, IfaceBuildError> {
            let fd = build(self.builder, true)?;

            trace!("creating TAP");

            Ok(UnboundEtherIface { fd })
        }

        /// Consume this `EtherIfaceBuilder` and build the TAP interface. The returned `EtherIface` object can be
        /// used to read/write ethernet frames from this interface. `handle` is a handle to a tokio
        /// event loop which will be used for reading/writing.
        pub fn build(self, handle: &Handle) -> Result<EtherIface, IfaceBuildError> {
            Ok(self.build_unbound()?.bind(handle))
        }
    }

    /// Represents a TAP device which has been built but not bound to a tokio event loop.
    #[derive(Debug)]
    pub struct UnboundEtherIface {
        fd: AsyncFd,
    }

    impl UnboundEtherIface {
        /// Bind the tap device to the event loop, creating a `EtherIface` which you can read/write ethernet
        /// frames with.
        pub fn bind(self, handle: &Handle) -> EtherIface {
            let UnboundEtherIface { fd } = self;
            let fd = unwrap!(PollEvented::new(fd, handle));
            EtherIface { fd }
        }
    }

    /// A handle to a virtual (TAP) network interface. Can be used to read/write ethernet frames
    /// directly to the device.
    pub struct EtherIface {
        fd: PollEvented<AsyncFd>,
    }

    impl Stream for EtherIface {
        type Item = EtherFrame;
        type Error = io::Error;
        
        fn poll(&mut self) -> io::Result<Async<Option<EtherFrame>>> {
            if let Async::NotReady = self.fd.poll_read() {
                return Ok(Async::NotReady);
            }

            let mut buffer: [u8; sys::ETH_FRAME_LEN as usize] = unsafe {
                mem::uninitialized()
            };
            match self.fd.read(&mut buffer[..]) {
                Ok(0) => Ok(Async::Ready(None)),
                Ok(n) => {

                    /*
                    'out: for i in 0.. {
                        println!("");
                        for j in 0..4 {
                            let pos = i * 4 + j;
                            if pos < n {
                                print!("{:02x}", buffer[pos]);
                            } else {
                                break 'out;
                            }
                        }
                    }
                    println!("");
                    */

                    let bytes = Bytes::from(&buffer[..n]);
                    let frame = EtherFrame::from_bytes(bytes);
                    info!("TAP sending frame: {:?}", frame);
                    Ok(Async::Ready(Some(frame)))
                },
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.fd.need_read();
                    Ok(Async::NotReady)
                },
                Err(e) => Err(e),
            }
        }
    }

    impl Sink for EtherIface {
        type SinkItem = EtherFrame;
        type SinkError = io::Error;
        
        fn start_send(&mut self, item: EtherFrame) -> io::Result<AsyncSink<EtherFrame>> {
            info!("TAP received frame: {:?}", item);
            if let Async::NotReady = self.fd.poll_write() {
                return Ok(AsyncSink::NotReady(item));
            }

            /*
            trace!("frame as bytes:");
            for chunk in item.as_bytes().chunks(8) {
                let mut s = String::new();
                for b in chunk {
                    use std::fmt::Write;
                    write!(&mut s, " {:02x}", b).unwrap();
                }
                trace!("   {}", s);
            }
            */

            match self.fd.write(item.as_bytes()) {
                Ok(n) => assert_eq!(n, item.as_bytes().len()),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.fd.need_write();
                    return Ok(AsyncSink::NotReady(item));
                }
                Err(e) => return Err(e),
            }
            trace!("sent: {:?}", item);
            Ok(AsyncSink::Ready)
        }

        fn poll_complete(&mut self) -> io::Result<Async<()>> {
            Ok(Async::Ready(()))
        }
    }

    #[cfg(test)]
    mod test {
        use priv_prelude::*;
        use spawn;
        use capabilities;

        #[test]
        fn build_tap_name_contains_nul() {
            let tap_builder = {
                EtherIfaceBuilder::new()
                .address(Ipv4Addr::random_global())
                .name("hello\0")
            };
            let res = tap_builder.build_unbound();
            match res {
                Err(IfaceBuildError::NameContainsNul) => (),
                x => panic!("unexpected result: {:?}", x),
            }
        }

        #[test]
        fn build_tap_duplicate_name() {
            let join_handle = spawn::new_namespace(|| {
                let tap_builder = {
                    EtherIfaceBuilder::new()
                    .address(Ipv4Addr::random_global())
                    .name("hello")
                };
                let _tap = unwrap!(tap_builder.build_unbound());
                
                let tap_builder = {
                    EtherIfaceBuilder::new()
                    .address(Ipv4Addr::random_global())
                    .name("hello")
                };
                match tap_builder.build_unbound() {
                    Err(IfaceBuildError::InterfaceAlreadyExists) => (),
                    res => panic!("unexpected result: {:?}", res),
                }
            });
            unwrap!(join_handle.join());
        }

        #[test]
        fn build_tap_permission_denied() {
            let join_handle = spawn::new_namespace(|| {
                unwrap!(unwrap!(capabilities::Capabilities::new()).apply());

                let tap_builder = EtherIfaceBuilder::new();
                match tap_builder.build_unbound() {
                    Err(IfaceBuildError::CreateIfacePermissionDenied) => (),
                    res => panic!("unexpected result: {:?}", res),
                }
            });
            unwrap!(join_handle.join());
        }

        /*
        #[test]
        #[ignore]   // currently fails :(
        fn tap_blocks_on_namespaced_side() {
            let _ = env_logger::init();

            const NUM_PACKETS: usize = 500000;
            let mut core = unwrap!(Core::new());
            let handle = core.handle();
            let addr = addrv4!("10.2.3.4:567");

            let res = core.run(future::lazy(move || {
                let (join_handle, tap) = spawn::on_subnet(&handle, SubnetV4::local_10(), move |_ip| {
                    let socket = unwrap!(std::net::UdpSocket::bind(addr!("0.0.0.0:0")));
                    unwrap!(socket.set_write_timeout(Some(Duration::from_secs(1))));
                    for _ in 0..NUM_PACKETS {
                        match socket.send_to(&[], &SocketAddr::V4(addr)) {
                        Ok(_) => (),
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            return;
                        },
                        Err(e) => panic!("unexpected io error: {}", e),
                    }
                }
                panic!("all packets got sent");
            });

            tap
            .into_future()
            .map_err(|(e, _)| panic!("tap read error: {}", e))
            .and_then(move |(frame_opt, tap)| {
                let frame = unwrap!(frame_opt);
                let mac_addr = ethernet::random_mac();
                let arp = {
                    let src_mac = frame.src_addr();
                    let arp = match frame.ethertype() {
                        EthernetProtocol::Arp => {
                            let frame_ref = EtherFrame::new(frame.as_ref());
                            ArpPacket::new(frame_ref.payload())
                        },
                        p => panic!("unexpected payload: {:?}", p),
                    };
                    assert_eq!(arp.source_hardware_addr(), src_mac.as_bytes());
                    let src_ip = Ipv4Addr::from(assert_len!(4, arp.source_protocol_addr()));
                    assert_eq!(arp.target_hardware_addr(), EthernetAddress::BROADCAST.as_bytes());
                    assert_eq!(arp.target_protocol_addr(), &addr.ip().octets());
                    ArpPacket::new_reply(
                        mac_addr,
                        *addr.ip(),
                        src_mac,
                        src_ip,
                    )
                };
                let frame = EtherFrame::new_arp(
                    mac_addr,
                    frame.src_addr(),
                    &arp,
                );

                tap
                .send(frame)
                .map_err(|e| panic!("tap write error: {}", e))
                .map(move |_tap| {
                    unwrap!(join_handle.join());
                })
            })
        }));
        res.void_unwrap()
    }
    */
}

