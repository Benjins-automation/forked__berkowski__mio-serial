//! # mio-serial - Serial port I/O for mio
//!
//! This crate provides a serial port implementation compatable with mio.
//!
//! **Windows support is present but largely untested by the author**
//!
//! ## Links
//!   - repo:  https://github.com/berkowski/mio-serial
//!   - docs:  https://docs.rs/mio-serial
#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

// Enums, Structs, and Traits from the serialport crate
pub use serialport::{
    // Enums
    ClearBuffer,
    DataBits,
    // Structs
    Error,
    ErrorKind,
    FlowControl,
    Parity,
    // Types
    Result,
    // Traits
    SerialPort,
    SerialPortBuilder,
    SerialPortInfo,
    StopBits,
};

// Re-export port-enumerating utility function.
pub use serialport::available_ports;

// Re-export creation of SerialPortBuilder objects
pub use serialport::new;

//
//
//

use mio::{event::Source, Interest, Registry, Token};
use std::convert::TryFrom;
use std::io::{Error as StdIoError, ErrorKind as StdIoErrorKind, Result as StdIoResult};
use std::time::Duration;

#[cfg(unix)]
mod os_prelude {
    pub use mio::unix::SourceFd;
    pub use nix::{self, libc};
    pub use serialport::TTYPort as NativeBlockingSerialPort;
    pub use std::os::unix::prelude::*;
}

#[cfg(windows)]
mod os_prelude {
    pub use serialport::COMPort as NativeBlockingSerialPort;
    pub use mio::windows::NamedPipe;
    pub use std::ffi::OsStr;
    pub use std::io::{self, Read, Write};
    pub use std::mem;
    pub use std::os::windows::ffi::OsStrExt;
    pub use std::os::windows::io::{FromRawHandle, RawHandle};
    pub use std::path::Path;
    pub use std::ptr;
    pub use std::time::Duration;
    pub use winapi::um::fileapi::*;
    pub use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    pub use winapi::um::winbase::{COMMTIMEOUTS, FILE_FLAG_OVERLAPPED};
    pub use winapi::um::winnt::{FILE_ATTRIBUTE_NORMAL, GENERIC_READ, GENERIC_WRITE, HANDLE};
    pub use winapi::um::commapi::SetCommTimeouts;
}
use os_prelude::*;

/// SerialStream
#[derive(Debug)]
pub struct SerialStream {
    #[cfg(unix)]
    inner: serialport::TTYPort,
    #[cfg(windows)]
    inner: serialport::COMPort,
    #[cfg(windows)]
    pipe: NamedPipe,
}

#[cfg(unix)]
fn map_nix_error(e: nix::Error) -> crate::Error {
    crate::Error {
        kind: crate::ErrorKind::Io(StdIoErrorKind::Other),
        description: e.to_string(),
    }
}

impl SerialStream {
    /// Open a nonblocking serial port from the provided builder
    ///
    /// ## Example
    ///
    /// ```ignore
    /// use std::path::Path;
    ///
    /// let path = "/dev/ttyUSB0";
    ///
    /// let serial = TTYSerial::open(path, 9600).unwrap();
    /// ```
    pub fn open(builder: &crate::SerialPortBuilder) -> crate::Result<Self> {
        #[cfg(unix)]
        {
            let port = NativeBlockingSerialPort::open(builder)?;
            Self::try_from(port)
        }
        #[cfg(windows)]
        {
            let (path, baud, parity, data_bits, stop_bits, flow_control) = {
                let com_port = serialport::COMPort::open(builder)?;
                let name = com_port.name().ok_or(crate::Error::new(
                    crate::ErrorKind::NoDevice,
                    "Empty device name",
                ))?;
                let baud = com_port.baud_rate()?;
                let parity = com_port.parity()?;
                let data_bits = com_port.data_bits()?;
                let stop_bits = com_port.stop_bits()?;
                let flow_control = com_port.flow_control()?;

                let mut path = Vec::<u16>::new();
                path.extend(OsStr::new("\\\\.\\").encode_wide());
                path.extend(Path::new(&name).as_os_str().encode_wide());
                path.push(0);

                (path, baud, parity, data_bits, stop_bits, flow_control)
            };

            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null_mut(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                    0 as HANDLE,
                )
            };

            if handle == INVALID_HANDLE_VALUE {
                return Err(crate::Error::from(StdIoError::last_os_error()));
            }
            let handle = unsafe { mem::transmute(handle) };

            // Construct NamedPipe and COMPort from Handle
            let pipe = unsafe { NamedPipe::from_raw_handle(handle) };
            let mut com_port = unsafe { serialport::COMPort::from_raw_handle(handle) };

            com_port.set_baud_rate(baud)?;
            com_port.set_parity(parity)?;
            com_port.set_data_bits(data_bits)?;
            com_port.set_stop_bits(stop_bits)?;
            com_port.set_flow_control(flow_control)?;
            sys::override_comm_timeouts(handle)?;

            Ok(Self {
                inner: com_port,
                pipe: pipe,
            })
        }
    }

    /// Create a pair of pseudo serial terminals
    ///
    /// ## Returns
    /// Two connected `Serial` objects: `(master, slave)`
    ///
    /// ## Errors
    /// Attempting any IO or parameter settings on the slave tty after the master
    /// tty is closed will return errors.
    ///
    /// ## Examples
    ///
    /// ```
    /// use mio_serial::SerialStream;
    ///
    /// let (master, slave) = SerialStream::pair().unwrap();
    /// ```
    #[cfg(unix)]
    pub fn pair() -> crate::Result<(Self, Self)> {
        let (master, slave) = NativeBlockingSerialPort::pair()?;

        let master = Self::try_from(master)?;
        let slave = Self::try_from(slave)?;

        Ok((master, slave))
    }

    /// Sets the exclusivity of the port
    ///
    /// If a port is exclusive, then trying to open the same device path again
    /// will fail.
    ///
    /// See the man pages for the tiocexcl and tiocnxcl ioctl's for more details.
    ///
    /// ## Errors
    ///
    /// * `Io` for any error while setting exclusivity for the port.
    #[cfg(unix)]
    pub fn set_exclusive(&mut self, exclusive: bool) -> crate::Result<()> {
        self.inner.set_exclusive(exclusive).map_err(|e| e)
    }

    /// Returns the exclusivity of the port
    ///
    /// If a port is exclusive, then trying to open the same device path again
    /// will fail.
    #[cfg(unix)]
    pub fn exclusive(&self) -> bool {
        self.inner.exclusive()
    }
}

impl crate::SerialPort for SerialStream {
    /// Start transmitting a break
    #[inline(always)]
    fn set_break(&self) -> crate::Result<()> {
        self.inner.set_break()
    }

    /// Stop transmitting a break
    #[inline(always)]
    fn clear_break(&self) -> crate::Result<()> {
        self.inner.clear_break()
    }

    /// Return the name associated with the serial port, if known.
    #[inline(always)]
    fn name(&self) -> Option<String> {
        self.inner.name()
    }

    /// Returns the current baud rate.
    ///
    /// This function returns `None` if the baud rate could not be determined. This may occur if
    /// the hardware is in an uninitialized state. Setting a baud rate with `set_baud_rate()`
    /// should initialize the baud rate to a supported value.
    #[inline(always)]
    fn baud_rate(&self) -> crate::Result<u32> {
        self.inner.baud_rate()
    }

    /// Returns the character size.
    ///
    /// This function returns `None` if the character size could not be determined. This may occur
    /// if the hardware is in an uninitialized state or is using a non-standard character size.
    /// Setting a baud rate with `set_char_size()` should initialize the character size to a
    /// supported value.
    #[inline(always)]
    fn data_bits(&self) -> crate::Result<crate::DataBits> {
        self.inner.data_bits()
    }

    /// Returns the flow control mode.
    ///
    /// This function returns `None` if the flow control mode could not be determined. This may
    /// occur if the hardware is in an uninitialized state or is using an unsupported flow control
    /// mode. Setting a flow control mode with `set_flow_control()` should initialize the flow
    /// control mode to a supported value.
    #[inline(always)]
    fn flow_control(&self) -> crate::Result<crate::FlowControl> {
        self.inner.flow_control()
    }

    /// Returns the parity-checking mode.
    ///
    /// This function returns `None` if the parity mode could not be determined. This may occur if
    /// the hardware is in an uninitialized state or is using a non-standard parity mode. Setting
    /// a parity mode with `set_parity()` should initialize the parity mode to a supported value.
    #[inline(always)]
    fn parity(&self) -> crate::Result<crate::Parity> {
        self.inner.parity()
    }

    /// Returns the number of stop bits.
    ///
    /// This function returns `None` if the number of stop bits could not be determined. This may
    /// occur if the hardware is in an uninitialized state or is using an unsupported stop bit
    /// configuration. Setting the number of stop bits with `set_stop-bits()` should initialize the
    /// stop bits to a supported value.
    #[inline(always)]
    fn stop_bits(&self) -> crate::Result<crate::StopBits> {
        self.inner.stop_bits()
    }

    /// Returns the current timeout. This parameter is const and equal to zero and implemented due
    /// to required for trait completeness.
    #[inline(always)]
    fn timeout(&self) -> Duration {
        Duration::from_secs(0)
    }

    // Port settings setters

    /// Sets the baud rate.
    ///
    /// ## Errors
    ///
    /// If the implementation does not support the requested baud rate, this function may return an
    /// `InvalidInput` error. Even if the baud rate is accepted by `set_baud_rate()`, it may not be
    /// supported by the underlying hardware.
    #[inline(always)]
    fn set_baud_rate(&mut self, baud_rate: u32) -> crate::Result<()> {
        self.inner.set_baud_rate(baud_rate)
    }

    /// Sets the character size.
    #[inline(always)]
    fn set_data_bits(&mut self, data_bits: crate::DataBits) -> crate::Result<()> {
        self.inner.set_data_bits(data_bits)
    }

    /// Sets the flow control mode.
    #[inline(always)]
    fn set_flow_control(&mut self, flow_control: crate::FlowControl) -> crate::Result<()> {
        self.inner.set_flow_control(flow_control)
    }

    /// Sets the parity-checking mode.
    #[inline(always)]
    fn set_parity(&mut self, parity: crate::Parity) -> crate::Result<()> {
        self.inner.set_parity(parity)
    }

    /// Sets the number of stop bits.
    #[inline(always)]
    fn set_stop_bits(&mut self, stop_bits: crate::StopBits) -> crate::Result<()> {
        self.inner.set_stop_bits(stop_bits)
    }

    /// Sets the timeout for future I/O operations. This parameter is ignored but
    /// required for trait completeness.
    #[inline(always)]
    fn set_timeout(&mut self, _: Duration) -> crate::Result<()> {
        Ok(())
    }

    // Functions for setting non-data control signal pins

    /// Sets the state of the RTS (Request To Send) control signal.
    ///
    /// Setting a value of `true` asserts the RTS control signal. `false` clears the signal.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the RTS control signal could not be set to the desired
    /// state on the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    #[inline(always)]
    fn write_request_to_send(&mut self, level: bool) -> crate::Result<()> {
        self.inner.write_request_to_send(level)
    }

    /// Writes to the Data Terminal Ready pin
    ///
    /// Setting a value of `true` asserts the DTR control signal. `false` clears the signal.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the DTR control signal could not be set to the desired
    /// state on the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    #[inline(always)]
    fn write_data_terminal_ready(&mut self, level: bool) -> crate::Result<()> {
        self.inner.write_data_terminal_ready(level)
    }

    // Functions for reading additional pins

    /// Reads the state of the CTS (Clear To Send) control signal.
    ///
    /// This function returns a boolean that indicates whether the CTS control signal is asserted.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the state of the CTS control signal could not be read
    /// from the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    #[inline(always)]
    fn read_clear_to_send(&mut self) -> crate::Result<bool> {
        self.inner.read_clear_to_send()
    }

    /// Reads the state of the Data Set Ready control signal.
    ///
    /// This function returns a boolean that indicates whether the DSR control signal is asserted.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the state of the DSR control signal could not be read
    /// from the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    #[inline(always)]
    fn read_data_set_ready(&mut self) -> crate::Result<bool> {
        self.inner.read_data_set_ready()
    }

    /// Reads the state of the Ring Indicator control signal.
    ///
    /// This function returns a boolean that indicates whether the RI control signal is asserted.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the state of the RI control signal could not be read from
    /// the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    #[inline(always)]
    fn read_ring_indicator(&mut self) -> crate::Result<bool> {
        self.inner.read_ring_indicator()
    }

    /// Reads the state of the Carrier Detect control signal.
    ///
    /// This function returns a boolean that indicates whether the CD control signal is asserted.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the state of the CD control signal could not be read from
    /// the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    #[inline(always)]
    fn read_carrier_detect(&mut self) -> crate::Result<bool> {
        self.inner.read_carrier_detect()
    }

    /// Gets the number of bytes available to be read from the input buffer.
    ///
    /// # Errors
    ///
    /// This function may return the following errors:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    #[inline(always)]
    fn bytes_to_read(&self) -> crate::Result<u32> {
        self.inner.bytes_to_read()
    }

    /// Get the number of bytes written to the output buffer, awaiting transmission.
    ///
    /// # Errors
    ///
    /// This function may return the following errors:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    #[inline(always)]
    fn bytes_to_write(&self) -> crate::Result<u32> {
        self.inner.bytes_to_write()
    }

    /// Discards all bytes from the serial driver's input buffer and/or output buffer.
    ///
    /// # Errors
    ///
    /// This function may return the following errors:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    #[inline(always)]
    fn clear(&self, buffer_to_clear: crate::ClearBuffer) -> crate::Result<()> {
        self.inner.clear(buffer_to_clear)
    }

    /// Attempts to clone the `SerialPort`. This allow you to write and read simultaneously from the
    /// same serial connection. Please note that if you want a real asynchronous serial port you
    /// should look at [mio-serial](https://crates.io/crates/mio-serial) or
    /// [tokio-serial](https://crates.io/crates/tokio-serial).
    ///
    /// Also, you must be very carefull when changing the settings of a cloned `SerialPort` : since
    /// the settings are cached on a per object basis, trying to modify them from two different
    /// objects can cause some nasty behavior.
    ///
    /// # Errors
    ///
    /// This function returns an error if the serial port couldn't be cloned.
    #[inline(always)]
    fn try_clone(&self) -> crate::Result<Box<dyn crate::SerialPort>> {
        Err(Error::new(
            ErrorKind::Io(StdIoErrorKind::Other),
            "SerialPort::try_clone not valid for SerialStream.",
        ))
        //self.inner.try_clone()
    }
}

impl TryFrom<NativeBlockingSerialPort> for SerialStream {
    type Error = crate::Error;
    #[cfg(unix)]
    fn try_from(port: NativeBlockingSerialPort) -> std::result::Result<Self, Self::Error> {
        use nix::sys::termios::{self, SetArg, SpecialCharacterIndices};
        let mut t = termios::tcgetattr(port.as_raw_fd()).map_err(map_nix_error)?;

        // Set VMIN = 1 to block until at least one character is received.
        t.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        termios::tcsetattr(port.as_raw_fd(), SetArg::TCSANOW, &t).map_err(map_nix_error)?;

        // Set the O_NONBLOCK flag.
        let flags = unsafe { libc::fcntl(port.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(StdIoError::last_os_error().into());
        }

        match unsafe { libc::fcntl(port.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } {
            0 => Ok(SerialStream { inner: port }),
            _ => Err(StdIoError::last_os_error().into()),
        }
    }
    #[cfg(windows)]
    fn try_from(_: NativeBlockingSerialPort) -> std::result::Result<Self, Self::Error> {
        unimplemented!()
    }
}

#[cfg(unix)]
mod io {
    use super::{SerialStream, StdIoError, StdIoErrorKind, StdIoResult};
    use nix::libc;
    use nix::sys::termios;
    use std::io::{Read, Write};
    use std::os::unix::prelude::*;

    macro_rules! uninterruptibly {
        ($e:expr) => {{
            loop {
                match $e {
                    Err(ref error) if error.kind() == StdIoErrorKind::Interrupted => {}
                    res => break res,
                }
            }
        }};
    }

    impl Read for SerialStream {
        fn read(&mut self, bytes: &mut [u8]) -> StdIoResult<usize> {
            uninterruptibly!(match unsafe {
                libc::read(
                    self.as_raw_fd(),
                    bytes.as_ptr() as *mut libc::c_void,
                    bytes.len() as libc::size_t,
                )
            } {
                x if x >= 0 => Ok(x as usize),
                _ => Err(StdIoError::last_os_error()),
            })
        }
    }

    impl Write for SerialStream {
        fn write(&mut self, bytes: &[u8]) -> StdIoResult<usize> {
            uninterruptibly!(match unsafe {
                libc::write(
                    self.as_raw_fd(),
                    bytes.as_ptr() as *const libc::c_void,
                    bytes.len() as libc::size_t,
                )
            } {
                x if x >= 0 => Ok(x as usize),
                _ => Err(StdIoError::last_os_error()),
            })
        }

        fn flush(&mut self) -> StdIoResult<()> {
            uninterruptibly!(termios::tcdrain(self.inner.as_raw_fd()).map_err(
                |error| match error {
                    nix::Error::Sys(errno) => StdIoError::from(errno),
                    error => StdIoError::new(StdIoErrorKind::Other, error.to_string()),
                }
            ))
        }
    }

    impl<'a> Read for &'a SerialStream {
        fn read(&mut self, bytes: &mut [u8]) -> StdIoResult<usize> {
            uninterruptibly!(match unsafe {
                libc::read(
                    self.as_raw_fd(),
                    bytes.as_ptr() as *mut libc::c_void,
                    bytes.len() as libc::size_t,
                )
            } {
                x if x >= 0 => Ok(x as usize),
                _ => Err(StdIoError::last_os_error()),
            })
        }
    }

    impl<'a> Write for &'a SerialStream {
        fn write(&mut self, bytes: &[u8]) -> StdIoResult<usize> {
            uninterruptibly!(match unsafe {
                libc::write(
                    self.as_raw_fd(),
                    bytes.as_ptr() as *const libc::c_void,
                    bytes.len() as libc::size_t,
                )
            } {
                x if x >= 0 => Ok(x as usize),
                _ => Err(StdIoError::last_os_error()),
            })
        }

        fn flush(&mut self) -> StdIoResult<()> {
            uninterruptibly!(termios::tcdrain(self.inner.as_raw_fd()).map_err(
                |error| match error {
                    nix::Error::Sys(errno) => StdIoError::from(errno),
                    error => StdIoError::new(StdIoErrorKind::Other, error.to_string()),
                }
            ))
        }
    }
}

#[cfg(windows)]
mod io {
    use super::{SerialStream, StdIoResult};
    use std::io::{Read, Write};

    impl Read for SerialStream {
        fn read(&mut self, bytes: &mut [u8]) -> StdIoResult<usize> {
            self.pipe.read(bytes)
        }
    }

    impl Write for SerialStream {
        fn write(&mut self, bytes: &[u8]) -> StdIoResult<usize> {
            self.pipe.write(bytes)
        }

        fn flush(&mut self) -> StdIoResult<()> {
            self.pipe.flush()
        }
    }
}

#[cfg(unix)]
mod sys {
    use super::{NativeBlockingSerialPort, SerialStream};
    use std::os::unix::prelude::*;

    impl AsRawFd for SerialStream {
        fn as_raw_fd(&self) -> RawFd {
            self.inner.as_raw_fd()
        }
    }

    impl IntoRawFd for SerialStream {
        fn into_raw_fd(self) -> RawFd {
            self.inner.into_raw_fd()
        }
    }

    impl FromRawFd for SerialStream {
        unsafe fn from_raw_fd(fd: RawFd) -> Self {
            let port = NativeBlockingSerialPort::from_raw_fd(fd);
            Self { inner: port }
        }
    }
}

#[cfg(windows)]
mod sys {

    use super::os_prelude::*;
    use super::StdIoResult;
    /// Overrides timeout value set by serialport-rs so that the read end will
    /// never wake up with 0-byte payload.
    pub(crate) fn override_comm_timeouts(handle: RawHandle) -> StdIoResult<()> {
        let mut timeouts = COMMTIMEOUTS {
            // wait at most 1ms between two bytes (0 means no timeout)
            ReadIntervalTimeout: 1,
            // disable "total" timeout to wait at least 1 byte forever
            ReadTotalTimeoutMultiplier: 0,
            ReadTotalTimeoutConstant: 0,
            // write timeouts are just copied from serialport-rs
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: 0,
        };

        let r = unsafe { SetCommTimeouts(handle, &mut timeouts) };
        if r == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Source for SerialStream {
    #[inline(always)]
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> StdIoResult<()> {
        SourceFd(&self.as_raw_fd()).register(registry, token, interests)
    }

    #[inline(always)]
    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> StdIoResult<()> {
        SourceFd(&self.as_raw_fd()).reregister(registry, token, interests)
    }

    #[inline(always)]
    fn deregister(&mut self, registry: &Registry) -> StdIoResult<()> {
        SourceFd(&self.as_raw_fd()).deregister(registry)
    }
}

#[cfg(windows)]
impl Source for SerialStream {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> StdIoResult<()> {
        self.pipe.register(registry, token, interest)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> StdIoResult<()> {
        self.pipe.reregister(registry, token, interest)
    }

    fn deregister(&mut self, registry: &Registry) -> StdIoResult<()> {
        self.pipe.deregister(registry)
    }
}

/// An extension trait for SerialPortBuilder
///
/// This trait adds an additional method to SerialPortBuilder:
///
/// - open_native_async
///
/// These methods mirror the `open` and `open_native` methods of SerialPortBuilder
pub trait SerialPortBuilderExt {
    /// Open a platform-specific interface to the port with the specified settings
    fn open_async(self) -> Result<SerialStream>;
}

impl SerialPortBuilderExt for SerialPortBuilder {
    /// Open a platform-specific interface to the port with the specified settings
    fn open_async(self) -> Result<SerialStream> {
        SerialStream::open(&self)
    }
}
