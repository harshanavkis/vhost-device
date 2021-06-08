#[macro_use(crate_version, crate_authors)]
extern crate clap;
extern crate option_parser;
extern crate vhost_user_vsock;

use clap::{App, Arg};
use option_parser::{OptionParser, OptionParserError};
use vhost_user_vsock::VsockConfig;

#[derive(Debug)]
pub enum ParserError {
    FailedConfigParse(OptionParserError),
    GuestCIDMissing,
    SocketParameterMissing,
}

fn main() {
    let vsock_args = App::new("vhost-user-vsock device server")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Launch a vhost-user-vsock device backend server")
        .arg(
            Arg::with_name("vsock-backend")
                .long("vsock-backend")
                .value_name("PARAMETERS")
                .help("\"guest-cid=<Context identifier of the guest which uniquely identifies the device for its lifetime.>,uds-path=<Unix socket to which a host-side application connects to>,socket=<Unix socket to which a hypervisor conencts to and sets up the control path with the device.>\"")
                .takes_value(true),
        )
        .get_matches();

    let backend_params = vsock_args.value_of("vsock-backend").unwrap();
    let vsock_config = parse_args(backend_params);

    match vsock_config {
        Ok(vsock) => println!(
            "{}, {}, {}",
            vsock.get_guest_cid(),
            vsock.get_socket_path(),
            vsock.get_uds_path()
        ),
        Err(e) => println!("{:?}", e),
    };
}

fn parse_args(backend_params: &str) -> Result<VsockConfig, ParserError> {
    let mut parser = OptionParser::new();
    parser.add("guest-cid").add("uds-path").add("socket");

    parser
        .parse(backend_params)
        .map_err(ParserError::FailedConfigParse)?;

    let guest_cid = parser
        .convert("guest-cid")
        .map_err(ParserError::FailedConfigParse)?
        .unwrap_or(3);
    let socket = parser
        .get("socket")
        .ok_or(ParserError::SocketParameterMissing)?;
    let uds_path = parser
        .get("uds-path")
        .ok_or(ParserError::SocketParameterMissing)?;

    Ok(VsockConfig::new(guest_cid, socket, uds_path))
}
