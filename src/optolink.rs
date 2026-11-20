use core::{
  pin::Pin,
  task::{Context, Poll},
};
use std::{
  fmt, io,
  net::{SocketAddr, ToSocketAddrs},
};

use pin_project::pin_project;
use serialport::{ClearBuffer, SerialPort};
use tokio::{
  io::{AsyncRead, AsyncWrite, ReadBuf},
  net::TcpStream,
};
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortBuilderExt, SerialStream, StopBits};

#[pin_project(project = DeviceProj)]
enum Device {
  Tty(#[pin] SerialStream, String),
  Stream(#[pin] TcpStream),
}

impl fmt::Debug for Device {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match self {
      Self::Tty(tty, _) => tty.fmt(f),
      Self::Stream(stream) => stream.fmt(f),
    }
  }
}

impl AsyncWrite for Device {
  fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<tokio::io::Result<usize>> {
    match self.project() {
      DeviceProj::Tty(tty, _) => tty.poll_write(cx, buf),
      DeviceProj::Stream(stream) => stream.poll_write(cx, buf),
    }
  }

  fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<tokio::io::Result<()>> {
    match self.project() {
      DeviceProj::Tty(tty, _) => tty.poll_flush(cx),
      DeviceProj::Stream(stream) => stream.poll_flush(cx),
    }
  }

  fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<tokio::io::Result<()>> {
    match self.project() {
      DeviceProj::Tty(tty, _) => tty.poll_shutdown(cx),
      DeviceProj::Stream(stream) => stream.poll_shutdown(cx),
    }
  }
}

impl AsyncRead for Device {
  fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<tokio::io::Result<()>> {
    let this = self.project();

    match this {
      DeviceProj::Tty(tty, _) => tty.poll_read(cx, buf),
      DeviceProj::Stream(stream) => stream.poll_read(cx, buf),
    }
  }
}

/// An Optolink connection via either a serial or TCP connection.
#[derive(Debug)]
#[pin_project]
pub struct Optolink {
  #[pin]
  device: Device,
}

impl Optolink {
  /// Opens a serial device, for this process only.
  ///
  /// serialport sets TIOCEXCL and takes `flock(LOCK_EX | LOCK_NB)` on the device. Root cannot bypass the `flock`, and
  /// the kernel releases it when the process ends, even by a crash. Another process holding the device makes this
  /// fail at once.
  ///
  /// # Examples
  ///
  /// ```no_run
  /// use vcontrol::Optolink;
  ///
  /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
  /// let mut device = Optolink::open("/dev/ttyUSB0").await?;
  /// # Ok(())
  /// # }
  /// ```
  pub async fn open(port: impl AsRef<str>) -> io::Result<Optolink> {
    log::trace!("Optolink::open(…)");

    let port = port.as_ref();

    let serial_port = tokio_serial::new(port, 4800)
      .data_bits(DataBits::Eight)
      .flow_control(FlowControl::None)
      .parity(Parity::Even)
      .stop_bits(StopBits::Two)
      .open_native_async();

    let serial_port = match serial_port {
      Ok(serial_port) => serial_port,
      Err(err) => {
        return match err.kind {
          // serialport reports EBUSY (TIOCEXCL) and a held `flock` as `NoDevice`; a missing device is `Io(NotFound)`.
          tokio_serial::ErrorKind::NoDevice => Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            format!("{port} is in use by another process (is vcontrol-mqtt running?)"),
          )),
          tokio_serial::ErrorKind::InvalidInput => Err(io::Error::new(io::ErrorKind::InvalidInput, err.description)),
          tokio_serial::ErrorKind::Unknown => Err(io::Error::other(err.description)),
          tokio_serial::ErrorKind::Io(kind) => Err(io::Error::new(kind, err.description)),
        };
      },
    };

    Ok(Optolink { device: Device::Tty(serial_port, port.to_owned()) })
  }

  /// Connects to a device via TCP.
  ///
  /// # Examples
  ///
  /// ```no_run
  /// use vcontrol::Optolink;
  ///
  /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
  /// let mut device = Optolink::connect(("localhost", 1234)).await?;
  /// # Ok(())
  /// # }
  /// ```
  pub async fn connect(addr: impl ToSocketAddrs) -> io::Result<Optolink> {
    log::trace!("Optolink::connect(…)");

    let addrs: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();

    let stream = TcpStream::connect(&addrs as &[SocketAddr]).await.map_err(|err| {
      io::Error::new(
        err.kind(),
        format!("{}: {}", err, addrs.iter().map(|addr| addr.to_string()).collect::<Vec<String>>().join(", ")),
      )
    })?;

    Ok(Optolink { device: Device::Stream(stream) })
  }

  /// Purge all contents of the input buffer.
  pub async fn purge(&mut self) -> Result<(), io::Error> {
    log::trace!("Optolink::purge()");

    match self.device {
      Device::Tty(ref mut tty, _) => Ok(tty.clear(ClearBuffer::Input)?),
      Device::Stream(ref mut stream) => {
        let mut buf = [0; 16];

        loop {
          match stream.try_read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
          }
        }

        Ok(())
      },
    }
  }

  pub async fn reinitialize(&mut self) -> Result<(), io::Error> {
    log::trace!("Optolink::reinitialize(…)");

    match self.device {
      Device::Tty(ref mut tty, ref port) => {
        use tokio::time::{Duration, sleep};

        // The device must stay locked against other processes throughout. An exclusive `flock` of the new stream
        // would conflict with the old one's, so both share it until the old one is closed.
        let _ = tty.set_exclusive(false);

        for _ in 0..10 {
          let serial_port = tokio_serial::new(port, 4800)
            .data_bits(DataBits::Eight)
            .flow_control(FlowControl::None)
            .parity(Parity::Even)
            .stop_bits(StopBits::Two)
            .exclusive(false)
            .open_native_async();

          if let Ok(serial_port) = serial_port {
            // Closes the old stream; the new one is now the only holder.
            *tty = serial_port;
            return Ok(tty.set_exclusive(true)?);
          }

          sleep(Duration::from_secs(1)).await;
        }

        Ok(tty.set_exclusive(true)?)
      },
      Device::Stream(_) => Ok(()),
    }
  }
}

impl AsyncWrite for Optolink {
  fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<tokio::io::Result<usize>> {
    log::trace!("Optolink::poll_write(…)");
    self.project().device.poll_write(cx, buf)
  }

  fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<tokio::io::Result<()>> {
    log::trace!("Optolink::poll_flush(…)");
    self.project().device.poll_flush(cx)
  }

  fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<tokio::io::Result<()>> {
    log::trace!("Optolink::poll_shutdown(…)");
    self.project().device.poll_shutdown(cx)
  }
}

impl AsyncRead for Optolink {
  fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<tokio::io::Result<()>> {
    log::trace!("Optolink::poll_read(…)");
    self.project().device.poll_read(cx, buf)
  }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
  use std::{ffi::CStr, fs::File, os::fd::FromRawFd};

  use super::*;

  /// A pseudo-terminal. Its slave stands in for the Optolink adapter.
  fn pty() -> (File, String) {
    unsafe {
      let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
      assert!(master >= 0);
      assert_eq!(libc::grantpt(master), 0);
      assert_eq!(libc::unlockpt(master), 0);
      let mut name = [0; 128];
      assert_eq!(libc::ptsname_r(master, name.as_mut_ptr(), name.len()), 0);
      (File::from_raw_fd(master), CStr::from_ptr(name.as_ptr()).to_str().unwrap().to_owned())
    }
  }

  fn assert_in_use(result: io::Result<Optolink>, device: &str) {
    let err = result.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ResourceBusy);
    assert_eq!(err.to_string(), format!("{device} is in use by another process (is vcontrol-mqtt running?)"));
  }

  /// The test runs as root in the devcontainer, but without CAP_SYS_ADMIN, so TIOCEXCL still applies. The `flock`
  /// alone is checked by `the_lock_holds_without_tiocexcl`.
  #[tokio::test]
  async fn only_one_optolink_can_open_a_device() {
    let (_master, device) = pty();
    let mut first = Optolink::open(&device).await.unwrap();
    assert_in_use(Optolink::open(&device).await, &device);

    // Reopens the serial stream, and keeps the device locked.
    let start = std::time::Instant::now();
    first.reinitialize().await.unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(1), "{:?}", start.elapsed());
    assert_in_use(Optolink::open(&device).await, &device);

    drop(first);
    Optolink::open(&device).await.unwrap();
  }

  /// Like root with CAP_SYS_ADMIN, which TIOCEXCL does not stop.
  #[tokio::test]
  async fn the_lock_holds_without_tiocexcl() {
    let (_master, device) = pty();
    let mut first = Optolink::open(&device).await.unwrap();
    first.reinitialize().await.unwrap();
    let Device::Tty(tty, _) = &first.device else { unreachable!() };
    let fd = std::os::fd::AsRawFd::as_raw_fd(tty);
    assert_eq!(unsafe { libc::ioctl(fd, libc::TIOCNXCL) }, 0);

    assert_in_use(Optolink::open(&device).await, &device);
  }

  #[tokio::test]
  async fn a_missing_device_is_not_reported_as_in_use() {
    let err = Optolink::open("/dev/nonexistent-optolink").await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
  }
}
