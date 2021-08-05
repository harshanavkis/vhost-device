# vhost-user-vsock

## Design

TODO: This section should have a high-level design of the crate.

Some questions that might help in writing this section:
- What is the purpose of this crate?
- What are the main components of the crate? How do they interact which each
  other?

## Usage

Run the vhost-user-vsock device:
```
vhost_user_vsock --vsock-backend guest-cid=4,uds-path=/tmp/vm4.vsock,socket=/tmp/vhost4.socket
```

Run qemu:

```
qemu-system-x86_64 -drive file=/path/to/disk.qcow2 -enable-kvm -m 512M \
  -smp 2 -vga virtio -chardev socket,id=char0,reconnect=0,path=/tmp/vhost4.socket \
  -device vhost-user-vsock-pci,chardev=char0 \
  -object memory-backend-file,share=on,id=mem,size="512M",mem-path="/dev/hugepages" \
  -numa node,memdev=mem -mem-prealloc
```

### Guest listening

#### iperf

```sh
# https://github.com/stefano-garzarella/iperf-vsock
guest$ iperf3 --vsock -s
host$  iperf3 --vsock -c /tmp/vm4.vsock
```

#### netcat

```sh
guest$ nc --vsock -l 1234

host$  nc -U /tmp/vm4.vsock
CONNECT 1234
```

### Host listening

#### iperf

```sh
# https://github.com/stefano-garzarella/iperf-vsock
host$  iperf3 --vsock -s -B /tmp/vm4.vsock
guest$ iperf3 --vsock -c 2
```

#### netcat

```sh
host$ nc -l -U /tmp/vm4.vsock_1234

guest$ nc --vsock 2 1234
```

```rust
use my_crate;

...
```

## License

**!!!NOTICE**: The BSD-3-Clause license is not included in this template.
The license needs to be manually added because the text of the license file
also includes the copyright. The copyright can be different for different
crates. If the crate contains code from CrosVM, the crate must add the
CrosVM copyright which can be found
[here](https://chromium.googlesource.com/chromiumos/platform/crosvm/+/master/LICENSE).
For crates developed from scratch, the copyright is different and depends on
the contributors.
