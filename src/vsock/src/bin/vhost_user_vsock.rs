#[macro_use(crate_version, crate_authors)]
extern crate clap;
extern crate vhost_user_vsock;

use clap::{App, Arg};
use vhost_user_vsock::start_backend_server;
use vhost_user_vsock::VsockConfig;

fn main() {
    let vsock_args = App::new("vhost-user-vsock device server")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Launch a vhost-user-vsock device backend server")
        .arg(Arg::with_name("guest-cid")
            .long("guest-cid")
            .help("Context identifier of the guest which uniquely identifies the device for its lifetime.")
            .takes_value(true))
        .arg(Arg::with_name("uds-path")
            .long("uds-path")
            .help("Unix socket to which a host-side application connects to.")
            .takes_value(true))
        .arg(Arg::with_name("socket")
            .long("socket")
            .help("Unix socket to which a hypervisor conencts to and sets up the control path with the device.")
            .takes_value(true))
        .get_matches();

    let guest_cid = match vsock_args.value_of("guest-cid") {
        Some(cid) => cid.parse().unwrap(),
        None => 3_u64,
    };

    let socket = vsock_args.value_of("socket").unwrap();
    let uds_path = vsock_args.value_of("uds-path").unwrap();

    let vsock_config = VsockConfig::new(guest_cid, socket.to_string(), uds_path.to_string());

    start_backend_server(vsock_config);
}
