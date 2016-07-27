use std::io::{Read, BufRead, BufReader, BufWriter, Cursor, Write, copy};
use std::net::{TcpStream, SocketAddr};
use std::string::String;
use std::str::FromStr;
use std::net::ToSocketAddrs;
use regex::Regex;
use chrono::{DateTime, UTC};
use chrono::offset::TimeZone;
#[cfg(feature = "secure")]
use openssl::ssl::{Ssl, SslContext, SslMethod, SslStream, IntoSsl};
use super::data_stream::DataStream;
use super::status;
use super::types::{FileType, FtpError, Line};

lazy_static! {
    // This regex extracts IP and Port details from PASV command response.
    // The regex looks for the pattern (h1,h2,h3,h4,p1,p2).
    static ref PORT_RE: Regex = Regex::new(r"\((\d+),(\d+),(\d+),(\d+),(\d+),(\d+)\)").unwrap();

    // This regex extracts modification time from MDTM command response.
    static ref MDTM_RE: Regex = Regex::new(r"\b(\d{4})(\d{2})(\d{2})(\d{2})(\d{2})(\d{2})\b").unwrap();

    // This regex extracts file size from SIZE command response.
    static ref SIZE_RE: Regex = Regex::new(r"\s+(\d+)\s*$").unwrap();
}

#[cfg(feature = "secure")]
lazy_static! {
    // Shared SSL context
    static ref SSL_CONTEXT: SslContext = match SslContext::new(SslMethod::Sslv23) {
        Ok(ctx) => ctx,
        Err(e) => panic!("{}", e)
    };
}

/// Stream to interface with the FTP server. This interface is only for the command stream.
#[derive(Debug)]
pub struct FtpStream {
    reader: BufReader<DataStream>,
}

impl FtpStream {
    /// Creates an FTP Stream.
    pub fn connect<A: ToSocketAddrs>(addr: A) -> Result<FtpStream, FtpError> {
        TcpStream::connect(addr)
            .map_err(|e| FtpError::ConnectionError(e))
            .and_then(|stream| {
                let mut ftp_stream = FtpStream {
                    reader: BufReader::new(DataStream::Tcp(stream))
                };
                ftp_stream.read_response(status::READY)
                    .map(|_| ftp_stream)
            })
    }

    /// Switch to secure mode if possible. If the connection is already
    /// secure does nothing.
    ///
    /// ## Panics
    ///
    /// Panics if the plain TCP connection cannot be switched to TLS mode.
    ///
    /// ## Example
    ///
    /// ```
    /// use ftp::FtpStream;
    /// let mut ftp_stream = FtpStream::connect("127.0.0.1:21").unwrap();
    /// // Switch to the secure mode
    /// let mut ftp_stream = ftp_stream.secure().unwrap();
    /// // Do all secret things
    /// let _ = ftp_stream.quit();
    /// ```
    ///
    #[cfg(feature = "secure")]
    pub fn secure(mut self) -> Result<FtpStream, FtpError> {
        // Initialize SSL with a default context and make secure the stream.
        Ssl::new(&SSL_CONTEXT)
            .map_err(|e| FtpError::SecureError(e.description().to_owned()))
            .and_then(|ssl| self.secure_with_ssl(ssl))
    }
   
    /// Switch to a secure mode if possible, using a provided SSL configuration.
    /// This method does nothing if the connect is already secured.
    ///
    /// ## Panics
    ///
    /// Panics if the plain TCP connection cannot be switched to TLS mode.
    ///
    /// ## Example
    ///
    /// ```
    /// use ftp::FtpStream;
    /// use openssl::ssl::*;
    ///
    /// // Create an SslContext with a custom cert.
    /// let mut ctx = SslContext::new(SslMethod::Sslv23).unwrap();
    /// let _ = ctx.set_CA_file("/path/to/a/cert.pem").unwrap();
    /// let mut ftp_stream = FtpStream::connect("127.0.0.1:21").unwrap();
    /// let mut ftp_stream = ftp_stream.secure_with_ssl(ctx).unwrap();
    /// ```
    #[cfg(feature = "secure")]
    pub fn secure_with_ssl<S: IntoSsl>(mut self, ssl: S) -> Result<FtpStream, FtpError> {
        // Do nothing if the connection is already secured.
        if self.reader.get_ref().is_ssl() {
            return Ok(self);
        }
        // Ask the server to start securing data.
        let auth_command = String::from("AUTH TLS\r\n");
        try!(self.write_str(&auth_command));
        try!(self.read_response(status::AUTH_OK));
        let stream = try!(SslStream::connect(ssl, self.reader.into_inner().into_tcp_stream())
                          .map_err(|e| FtpError::SecureError(e.description().to_owned())));
        let mut secured_ftp_tream = FtpStream {
            reader: BufReader::new(DataStream::Ssl(stream)),
        };
        // Set protection buffer size
        let pbsz_command = format!("PBSZ 0\r\n");
        try!(secured_ftp_tream.write_str(&pbsz_command));
        try!(secured_ftp_tream.read_response(status::COMMAND_OK));
        // Change the level of data protectio to Private
        let prot_command = String::from("PROT P\r\n");
        try!(secured_ftp_tream.write_str(&prot_command));
        try!(secured_ftp_tream.read_response(status::COMMAND_OK));
        Ok(secured_ftp_tream)
    }

    /// Switch to insecure mode. If the connection is already
    /// insecure does nothing.
    ///
    /// ## Example
    ///
    /// ```
    /// use ftp::FtpStream;
    /// let mut ftp_stream = FtpStream::connect("127.0.0.1:21").unwrap();
    /// // Switch to the secure mode
    /// let (mut ftp_stream, _) = ftp_stream.secure();
    /// // Do all secret things
    /// // Switch back to the insecure mode
    /// let (mut ftp_stream, _) = ftp_stream.insecure();
    /// // Do all public things
    /// let _ = ftp_stream.quit();
    /// ```
    ///
    #[cfg(feature = "secure")]
    pub fn insecure(mut self) -> Result<FtpStream, FtpError> {
        if !self.reader.get_ref().is_ssl() {
            return Ok(self);
        }
        // Ask the server to stop securing data
        let ccc_command = String::from("CCC\r\n");
        try!(self.write_str(&ccc_command));
        try!(self.read_response(status::COMMAND_OK));
        let plain_ftp_stream = FtpStream {
            reader: BufReader::new(DataStream::Tcp(self.reader.into_inner().into_tcp_stream())),
        };
        Ok(plain_ftp_stream)
    }

    /// Log in to the FTP server.
    pub fn login(&mut self, user: &str, password: &str) -> Result<(), FtpError> {
        let user_command = format!("USER {}\r\n", user);
        try!(self.write_str(&user_command));
        self.read_response_in(&[status::LOGGED_IN, status::NEED_PASSWORD])
            .and_then(|Line(code, _)| {
                if code == status::NEED_PASSWORD {
                    let pass_command = format!("PASS {}\r\n", password);
                    try!(self.write_str(&pass_command));
                    try!(self.read_response(status::LOGGED_IN));
                }
                Ok(())
            })
    }

    /// Change the current directory to the path specified.
    pub fn cwd(&mut self, path: &str) -> Result<(), FtpError> {
        let cwd_command = format!("CWD {}\r\n", path);
        try!(self.write_str(&cwd_command));
        self.read_response(status::REQUESTED_FILE_ACTION_OK).map(|_| ())
    }

    /// Move the current directory to the parent directory.
    pub fn cdup(&mut self) -> Result<(), FtpError> {
        let cdup_command = format!("CDUP\r\n");
        try!(self.write_str(&cdup_command));
        self.read_response(status::REQUESTED_FILE_ACTION_OK).map(|_| ())
    }

    /// Gets the current directory
    pub fn pwd(&mut self) -> Result<String, FtpError> {
        try!(self.write_str("PWD\r\n"));
        self.read_response(status::PATH_CREATED)
            .and_then(|Line(_, content)| {
                match (content.find('"'), content.rfind('"')) {
                    (Some(begin), Some(end)) if begin < end => {
                        Ok(content[begin + 1 .. end].to_string())
                    },
                    _ => {
                        let cause = format!("Invalid PWD Response: {}", content);
                        Err(FtpError::InvalidResponse(cause))
                    }
                }
            })
    }

    /// This does nothing. This is usually just used to keep the connection open.
    pub fn noop(&mut self) -> Result<(), FtpError> {
        let noop_command = format!("NOOP\r\n");
        try!(self.write_str(&noop_command));
        self.read_response(status::COMMAND_OK).map(|_| ())
    }

    /// This creates a new directory on the server.
    pub fn mkdir(&mut self, pathname: &str) -> Result<(), FtpError> {
        let mkdir_command = format!("MKD {}\r\n", pathname);
        try!(self.write_str(&mkdir_command));
        self.read_response(status::PATH_CREATED).map(|_| ())
    }

    /// Runs the PASV command.
    fn pasv(&mut self) -> Result<SocketAddr, FtpError> {
        try!(self.write_str("PASV\r\n"));
        // PASV response format : 227 Entering Passive Mode (h1,h2,h3,h4,p1,p2).
        let Line(_, line) = try!(self.read_response(status::PASSIVE_MODE));
        PORT_RE.captures(&line)
            .ok_or(FtpError::InvalidResponse(format!("Invalid PASV response: {}", line)))
            .and_then(|caps| {
                // If the regex matches we can be sure groups contains numbers
                let (oct1, oct2, oct3, oct4) = (
                    caps[1].parse::<u8>().unwrap(),
                    caps[2].parse::<u8>().unwrap(),
                    caps[3].parse::<u8>().unwrap(),
                    caps[4].parse::<u8>().unwrap()
                );
                let (msb, lsb) = (
                    caps[5].parse::<u8>().unwrap(),
                    caps[6].parse::<u8>().unwrap()
                );
                let port = ((msb as u16) << 8) + lsb as u16;
                let addr = format!("{}.{}.{}.{}:{}", oct1, oct2, oct3, oct4, port);
                SocketAddr::from_str(&addr)
                    .map_err(|parse_err| FtpError::InvalidAddress(parse_err))
            })
    }

    // Execute command which send data back in a separate stream
    #[cfg(not(feature = "secure"))]
    fn data_command(&mut self, cmd: &str) -> Result<DataStream, FtpError> {
        self.pasv()
            .and_then(|addr| self.write_str(cmd).map(|_| addr))
            .and_then(|addr| TcpStream::connect(addr)
                      .map_err(|e| FtpError::ConnectionError(e)))
            .map(|stream| DataStream::Tcp(stream))
    }

    #[cfg(feature = "secure")]
    fn data_command(&mut self, cmd: &str) -> Result<DataStream, FtpError> {
        self.pasv()
            .and_then(|addr| self.write_str(cmd).map(|_| addr))
            .and_then(|addr| TcpStream::connect(addr))
            .and_then(|stream| {
                if self.reader.get_ref().is_ssl() {
                    Ssl::new(&SSL_CONTEXT)
                        .and_then(|ssl| SslStream::connect(ssl, stream))
                        .map(|stream| DataStream::Ssl(stream))
                        .map_err(|e| FtpError::SecureError(e.description().to_owned()))
                } else {
                    Ok(DataStream::Tcp(stream))
                }
            })
    }

    /// Sets the type of file to be transferred. That is the implementation
    /// of `TYPE` command.
    pub fn transfer_type(&mut self, file_type: FileType) -> Result<(), FtpError> {
        let type_command = format!("TYPE {}\r\n", file_type.to_string());
        try!(self.write_str(&type_command));
        self.read_response(status::COMMAND_OK).map(|_| ())
    }

    /// Quits the current FTP session.
    pub fn quit(&mut self) -> Result<(), FtpError> {
        let quit_command = format!("QUIT\r\n");
        try!(self.write_str(&quit_command));
        self.read_response(status::CLOSING).map(|_| ())
    }

    /// Retrieves the file name specified from the server.
    /// This method is a more complicated way to retrieve a file.
    /// The reader returned should be dropped.
    /// Also you will have to read the response to make sure it has the correct value.
    pub fn get(&mut self, file_name: &str) -> Result<BufReader<DataStream>, FtpError> {
        let retr_command = format!("RETR {}\r\n", file_name);
        let data_stream = BufReader::new(try!(self.data_command(&retr_command)));
        self.read_response(status::ABOUT_TO_SEND).map(|_| data_stream)
    }

    /// Renames the file from_name to to_name
    pub fn rename(&mut self, from_name: &str, to_name: &str) -> Result<(), FtpError> {
        let rnfr_command = format!("RNFR {}\r\n", from_name);
        try!(self.write_str(&rnfr_command));
        self.read_response(status::REQUEST_FILE_PENDING)
            .and_then(|_| {
                let rnto_command = format!("RNTO {}\r\n", to_name);
                try!(self.write_str(&rnto_command));
                self.read_response(status::REQUESTED_FILE_ACTION_OK).map(|_| ())
            })
    }

    /// The implementation of `RETR` command where `filename` is the name of the file
    /// to download from FTP and `reader` is the function which operates with the
    /// data stream opened.
    ///
    /// ```ignore
    /// let result = conn.retr("take_this.txt", |stream| {
    ///   let mut file = File::create("store_here.txt").unwrap();
    ///   let mut buf = [0; 2048];
    ///
    ///   loop {
    ///     match stream.read(&mut buf) {
    ///       Ok(0) => break,
    ///       Ok(n) => file.write_all(&buf[0..n]).unwrap(),
    ///       Err(err) => return Err(err)
    ///     };
    ///   }
    ///
    ///   Ok(())
    /// });
    /// ```
    pub fn retr<F>(&mut self, filename: &str, reader: F) -> Result<(), FtpError>
    where F: Fn(&mut Read) -> Result<(), FtpError> {
        let retr_command = format!("RETR {}\r\n", filename);
        let mut data_stream = BufReader::new(try!(self.data_command(&retr_command)));
        self.read_response_in(&[status::ABOUT_TO_SEND, status::ALREADY_OPEN])
            .and_then(|_| {
                let result = reader(&mut data_stream);
                drop(data_stream);
                try!(self.read_response(status::CLOSING_DATA_CONNECTION));
                result
            })
    }

    fn simple_retr_(&mut self, file_name: &str) -> Result<Cursor<Vec<u8>>, FtpError> {
        let mut data_stream = try!(self.get(file_name));
        let buffer: &mut Vec<u8> = &mut Vec::new();
        loop {
            let mut buf = [0; 256];
            let len = try!(data_stream.read(&mut buf).map_err(|read_err| FtpError::ConnectionError(read_err)));
            if len == 0 {
                break;
            }
            try!(buffer.write(&buf[0..len]).map_err(|write_err| FtpError::ConnectionError(write_err)));
        }
        drop(data_stream);
        Ok(Cursor::new(buffer.clone()))
    }

    /// Simple way to retr a file from the server. This stores the file in memory.
    pub fn simple_retr(&mut self, file_name: &str) -> Result<Cursor<Vec<u8>>, FtpError> {
        let r = try!(self.simple_retr_(file_name));
        self.read_response(status::CLOSING_DATA_CONNECTION).map(|_| r)
    }

    /// Removes the remote pathname from the server.
    pub fn rmdir(&mut self, pathname: &str) -> Result<(), FtpError> {
        let rmd_command = format!("RMD {}\r\n", pathname);
        try!(self.write_str(&rmd_command));
        self.read_response(status::REQUESTED_FILE_ACTION_OK).map(|_| ())
    }

    /// Remove the remote file from the server.
    pub fn rm(&mut self, filename: &str) -> Result<(), FtpError> {
        let rm_command = format!("DELE {}\r\n", filename);
        try!(self.write_str(&rm_command));
        self.read_response(status::REQUESTED_FILE_ACTION_OK).map(|_| ())
    }

    fn put_file<R: Read>(&mut self, filename: &str, r: &mut R) -> Result<(), FtpError> {
        let stor_command = format!("STOR {}\r\n", filename);
        let mut data_stream = BufWriter::new(try!(self.data_command(&stor_command)));
        try!(self.read_response_in(&[status::ALREADY_OPEN, status::ABOUT_TO_SEND]));
        copy(r, &mut data_stream)
            .map_err(|read_err| FtpError::ConnectionError(read_err))
            .map(|_| ())
    }

    /// This stores a file on the server.
    pub fn put<R: Read>(&mut self, filename: &str, r: &mut R) -> Result<(), FtpError> {
        try!(self.put_file(filename, r));
        self.read_response(status::CLOSING_DATA_CONNECTION).map(|_| ())
    }

    /// Execute a command which returns list of strings in a separate stream
    fn list_command(&mut self, cmd: String, open_code: u32, close_code: u32) -> Result<Vec<String>, FtpError> {
        let mut data_stream = BufReader::new(try!(self.data_command(&cmd)));
        try!(self.read_response_in(&[open_code, status::ALREADY_OPEN]));
        
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::new();
        loop {
            match data_stream.read_to_string(&mut line) {
                Ok(0) => break,
                Ok(_) => lines.extend(line.split("\r\n").into_iter().map(|s| String::from(s)).filter(|s| s.len() > 0)),
                Err(err) => return Err(FtpError::ConnectionError(err)),
            };
        }

        self.read_response(close_code).map(|_| lines)
    }

    /// Execute `LIST` command which returns the detailed file listing in human readable format.
    /// If `pathname` is omited then the list of files in the current directory will be
    /// returned otherwise it will the list of files on `pathname`.
    pub fn list(&mut self, pathname: Option<&str>) -> Result<Vec<String>, FtpError> {
        let command = match pathname {
            Some(path) => format!("LIST {}\r\n", path),
            None => String::from("LIST\r\n"),
        };

        self.list_command(command, status::ABOUT_TO_SEND, status::CLOSING_DATA_CONNECTION)
    }

    /// Execute `NLST` command which returns the list of file names only.
    /// If `pathname` is omited then the list of files in the current directory will be
    /// returned otherwise it will the list of files on `pathname`.
    pub fn nlst(&mut self, pathname: Option<&str>) -> Result<Vec<String>, FtpError> {
        let command = match pathname {
            Some(path) => format!("NLST {}\r\n", path),
            None => String::from("NLST\r\n"),
        };

        self.list_command(command, status::ABOUT_TO_SEND, status::CLOSING_DATA_CONNECTION)
    }

    /// Retrieves the modification time of the file at `pathname` if it exists.
    /// In case the file does not exist `None` is returned.
    pub fn mdtm(&mut self, pathname: &str) -> Result<Option<DateTime<UTC>>, FtpError> {
        let mdtm_command = format!("MDTM {}\r\n", pathname);
        try!(self.write_str(&mdtm_command));
        let Line(_, content) = try!(self.read_response(status::FILE));

        match MDTM_RE.captures(&content) {
            Some(caps) => {
                let (year, month, day) = (
                    caps[1].parse::<i32>().unwrap(),
                    caps[2].parse::<u32>().unwrap(),
                    caps[3].parse::<u32>().unwrap()
                );
                let (hour, minute, second) = (
                    caps[4].parse::<u32>().unwrap(),
                    caps[5].parse::<u32>().unwrap(),
                    caps[6].parse::<u32>().unwrap()
                );
                Ok(Some(UTC.ymd(year, month, day).and_hms(hour, minute, second)))
            },
            None => Ok(None)
        }
    }

    /// Retrieves the size of the file in bytes at `pathname` if it exists.
    /// In case the file does not exist `None` is returned.
    pub fn size(&mut self, pathname: &str) -> Result<Option<usize>, FtpError> {
        let size_command = format!("SIZE {}\r\n", pathname);
        try!(self.write_str(&size_command));
        let Line(_, content) = try!(self.read_response(status::FILE));

        match SIZE_RE.captures(&content) {
            Some(caps) => Ok(Some(caps[1].parse().unwrap())),
            None => Ok(None)
        }
    }

    fn write_str(&mut self, s: &str) -> Result<(), FtpError> {
        let stream = self.reader.get_mut();

        if cfg!(feature = "debug_print") {
            print!("CMD {}", s);
        }

        stream.write_fmt(format_args!("{}", s))
            .map_err(|send_err| FtpError::ConnectionError(send_err))
    }

    pub fn read_response(&mut self, expected_code: u32) -> Result<Line, FtpError> {
        self.read_response_in(&[expected_code])
    }

    /// Retrieve single line response
    pub fn read_response_in(&mut self, expected_code: &[u32]) -> Result<Line, FtpError> {
        let mut line = String::new();
        try!(self.reader.read_line(&mut line)
             .map_err(|read_err| FtpError::ConnectionError(read_err)));

        if cfg!(feature = "debug_print") {
            print!("FTP {}", line);
        }

        if line.len() < 5 {
            return Err(FtpError::InvalidResponse("error: could not read reply code".to_owned()));
        }

        let code: u32 = try!(line[0..3].parse()
                             .map_err(|err| {
                                 FtpError::InvalidResponse(format!("error: could not parse reply code: {}", err))
                             }));

        // multiple line reply
        // loop while the line does not begin with the code and a space
        let expected = format!("{} ", &line[0..3]);
        while line.len() < 5 || line[0..4] != expected {
            line.clear();
            if let Err(e) = self.reader.read_line(&mut line) {
                return Err(FtpError::ConnectionError(e));
            }

            if cfg!(feature = "debug_print") {
                print!("FTP {}", line);
            }
        }

        if expected_code.into_iter().any(|ec| code == *ec) {
            Ok(Line(code, line))
        } else {
            Err(FtpError::InvalidResponse(format!("Expected code {:?}, got response: {}", expected_code, line)))
        }
    }
}
