use std::collections::VecDeque;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const METHOD_CALL: u8 = 1;
const METHOD_RETURN: u8 = 2;
const ERROR: u8 = 3;
const SIGNAL: u8 = 4;

const DBUS_DESTINATION: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const FILE_CHOOSER_INTERFACE: &str = "org.freedesktop.portal.FileChooser";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

const MAX_MESSAGE_SIZE: usize = 128 * 1024 * 1024;
const METHOD_TIMEOUT: Duration = Duration::from_secs(10);

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn align(position: usize, boundary: usize) -> io::Result<usize> {
    debug_assert!(boundary.is_power_of_two());
    position
        .checked_add(boundary - 1)
        .map(|value| value & !(boundary - 1))
        .ok_or_else(|| invalid("D-Bus size overflow"))
}

#[derive(Debug, Clone)]
pub struct OpenFileOptions {
    pub title: String,
    pub accept_label: Option<String>,
    pub multiple: bool,
    pub directory: bool,
    pub current_folder: Option<PathBuf>,
    pub parent_window: String,
}

impl Default for OpenFileOptions {
    fn default() -> Self {
        Self {
            title: "Open File".into(),
            accept_label: None,
            multiple: false,
            directory: false,
            current_folder: None,
            parent_window: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SaveFileOptions {
    pub title: String,
    pub accept_label: Option<String>,
    pub current_name: Option<String>,
    pub current_folder: Option<PathBuf>,
    pub parent_window: String,
}

impl Default for SaveFileOptions {
    fn default() -> Self {
        Self {
            title: "Save File".into(),
            accept_label: None,
            current_name: None,
            current_folder: None,
            parent_window: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChooserResponse {
    Selected(Vec<PathBuf>),
    Cancelled,
    Other,
}

#[derive(Debug, Clone)]
enum FileChooserRequest {
    Open(OpenFileOptions),
    Save(SaveFileOptions),
}

impl FileChooserRequest {
    fn member(&self) -> &'static str {
        match self {
            Self::Open(_) => "OpenFile",
            Self::Save(_) => "SaveFile",
        }
    }

    fn encode_body(&self, token: &str, message: &mut Encoder) -> io::Result<()> {
        match self {
            Self::Open(options) => {
                message.string(&options.parent_window)?;
                message.string(&options.title)?;
                message.dict(|message| {
                    message.option_string("handle_token", token)?;
                    if let Some(label) = &options.accept_label {
                        message.option_string("accept_label", label)?;
                    }
                    message.option_bool("modal", true)?;
                    message.option_bool("multiple", options.multiple)?;
                    message.option_bool("directory", options.directory)?;
                    if let Some(folder) = &options.current_folder {
                        message.option_path("current_folder", folder)?;
                    }
                    Ok(())
                })
            }
            Self::Save(options) => {
                message.string(&options.parent_window)?;
                message.string(&options.title)?;
                message.dict(|message| {
                    message.option_string("handle_token", token)?;
                    if let Some(label) = &options.accept_label {
                        message.option_string("accept_label", label)?;
                    }
                    message.option_bool("modal", true)?;
                    if let Some(name) = &options.current_name {
                        message.option_string("current_name", name)?;
                    }
                    if let Some(folder) = &options.current_folder {
                        message.option_path("current_folder", folder)?;
                    }
                    Ok(())
                })
            }
        }
    }
}

pub fn open_files(options: OpenFileOptions) -> io::Result<FileChooserResponse> {
    file_chooser(FileChooserRequest::Open(options))
}

pub fn save_file(options: SaveFileOptions) -> io::Result<FileChooserResponse> {
    file_chooser(FileChooserRequest::Save(options))
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(256),
        }
    }

    fn align(&mut self, boundary: usize) -> io::Result<()> {
        self.bytes.resize(align(self.bytes.len(), boundary)?, 0);
        Ok(())
    }

    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) -> io::Result<()> {
        self.align(4)?;
        self.bytes.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn i32(&mut self, value: i32) -> io::Result<()> {
        self.u32(value as u32)
    }

    fn boolean(&mut self, value: bool) -> io::Result<()> {
        self.u32(u32::from(value))
    }

    fn string(&mut self, value: &str) -> io::Result<()> {
        if value.as_bytes().contains(&0) {
            return Err(invalid("D-Bus strings cannot contain NUL bytes"));
        }
        let length = u32::try_from(value.len()).map_err(|_| invalid("D-Bus string too long"))?;
        self.u32(length)?;
        self.bytes.extend_from_slice(value.as_bytes());
        self.byte(0);
        Ok(())
    }

    fn signature(&mut self, value: &str) -> io::Result<()> {
        let length = u8::try_from(value.len()).map_err(|_| invalid("D-Bus signature too long"))?;
        self.byte(length);
        self.bytes.extend_from_slice(value.as_bytes());
        self.byte(0);
        Ok(())
    }

    fn field(&mut self, code: u8, value_type: &str, value: &str) -> io::Result<()> {
        self.align(8)?;
        self.byte(code);
        self.signature(value_type)?;
        match value_type {
            "g" => self.signature(value),
            "o" | "s" => self.string(value),
            _ => Err(invalid("unsupported D-Bus header type")),
        }
    }

    fn array(
        &mut self,
        element_alignment: usize,
        body: impl FnOnce(&mut Encoder) -> io::Result<()>,
    ) -> io::Result<()> {
        self.align(4)?;
        let length_position = self.bytes.len();
        self.bytes.extend_from_slice(&0u32.to_le_bytes());
        self.align(element_alignment)?;
        let body_start = self.bytes.len();
        body(self)?;
        let length = u32::try_from(self.bytes.len() - body_start)
            .map_err(|_| invalid("D-Bus array too large"))?;
        self.bytes[length_position..length_position + 4].copy_from_slice(&length.to_le_bytes());
        Ok(())
    }

    fn dict(&mut self, body: impl FnOnce(&mut Encoder) -> io::Result<()>) -> io::Result<()> {
        self.array(8, body)
    }

    fn option(
        &mut self,
        key: &str,
        value_signature: &str,
        value: impl FnOnce(&mut Encoder) -> io::Result<()>,
    ) -> io::Result<()> {
        self.align(8)?;
        self.string(key)?;
        self.signature(value_signature)?;
        value(self)
    }

    fn option_string(&mut self, key: &str, value: &str) -> io::Result<()> {
        self.option(key, "s", |message| message.string(value))
    }

    fn option_bool(&mut self, key: &str, value: bool) -> io::Result<()> {
        self.option(key, "b", |message| message.boolean(value))
    }

    fn option_path(&mut self, key: &str, value: &Path) -> io::Result<()> {
        let bytes = value.as_os_str().as_bytes();
        if bytes.contains(&0) {
            return Err(invalid("filesystem paths cannot contain NUL bytes"));
        }
        self.option(key, "ay", |message| {
            message.array(1, |message| {
                message.bytes.extend_from_slice(bytes);
                message.byte(0);
                Ok(())
            })
        })
    }
}

fn message(
    kind: u8,
    serial: u32,
    path: Option<&str>,
    interface: Option<&str>,
    member: Option<&str>,
    destination: Option<&str>,
    signature: Option<&str>,
    body: impl FnOnce(&mut Encoder) -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    if serial == 0 {
        return Err(invalid("D-Bus message serial cannot be zero"));
    }

    let mut message = Encoder::new();
    message.bytes.extend_from_slice(&[b'l', kind, 0, 1]);
    message.u32(0)?;
    message.u32(serial)?;
    message.u32(0)?;

    let fields_start = message.bytes.len();
    if let Some(path) = path {
        message.field(1, "o", path)?;
    }
    if let Some(interface) = interface {
        message.field(2, "s", interface)?;
    }
    if let Some(member) = member {
        message.field(3, "s", member)?;
    }
    if let Some(destination) = destination {
        message.field(6, "s", destination)?;
    }
    if let Some(signature) = signature {
        message.field(8, "g", signature)?;
    }
    let fields_len = u32::try_from(message.bytes.len() - fields_start)
        .map_err(|_| invalid("D-Bus header too large"))?;
    message.bytes[12..16].copy_from_slice(&fields_len.to_le_bytes());

    message.align(8)?;
    let body_start = message.bytes.len();
    body(&mut message)?;
    let body_len = u32::try_from(message.bytes.len() - body_start)
        .map_err(|_| invalid("D-Bus body too large"))?;
    message.bytes[4..8].copy_from_slice(&body_len.to_le_bytes());
    if message.bytes.len() > MAX_MESSAGE_SIZE {
        return Err(invalid("D-Bus message exceeds 128 MiB"));
    }
    Ok(message.bytes)
}

fn method_call(
    serial: u32,
    path: &str,
    interface: &str,
    member: &str,
    destination: &str,
    signature: Option<&str>,
    body: impl FnOnce(&mut Encoder) -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    message(
        METHOD_CALL,
        serial,
        Some(path),
        Some(interface),
        Some(member),
        Some(destination),
        signature,
        body,
    )
}

fn hello_message(serial: u32) -> io::Result<Vec<u8>> {
    method_call(
        serial,
        DBUS_PATH,
        DBUS_DESTINATION,
        "Hello",
        DBUS_DESTINATION,
        None,
        |_| Ok(()),
    )
}

fn start_service_message(serial: u32, service: &str) -> io::Result<Vec<u8>> {
    method_call(
        serial,
        DBUS_PATH,
        DBUS_DESTINATION,
        "StartServiceByName",
        DBUS_DESTINATION,
        Some("su"),
        |message| {
            message.string(service)?;
            message.u32(0)
        },
    )
}

fn get_name_owner_message(serial: u32, service: &str) -> io::Result<Vec<u8>> {
    method_call(
        serial,
        DBUS_PATH,
        DBUS_DESTINATION,
        "GetNameOwner",
        DBUS_DESTINATION,
        Some("s"),
        |message| message.string(service),
    )
}

fn add_match_message(serial: u32, rule: &str) -> io::Result<Vec<u8>> {
    method_call(
        serial,
        DBUS_PATH,
        DBUS_DESTINATION,
        "AddMatch",
        DBUS_DESTINATION,
        Some("s"),
        |message| message.string(rule),
    )
}

fn file_chooser_message(
    serial: u32,
    request: &FileChooserRequest,
    token: &str,
) -> io::Result<Vec<u8>> {
    method_call(
        serial,
        PORTAL_PATH,
        FILE_CHOOSER_INTERFACE,
        request.member(),
        PORTAL_DESTINATION,
        Some("ssa{sv}"),
        |message| request.encode_body(token, message),
    )
}

fn notification_message(serial: u32, title: &str, body: &str) -> io::Result<Vec<u8>> {
    method_call(
        serial,
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        "org.freedesktop.Notifications",
        Some("susssasa{sv}i"),
        |message| {
            message.string("rust-raw-dbus")?;
            message.u32(0)?;
            message.string("")?;
            message.string(title)?;
            message.string(body)?;
            message.array(4, |_| Ok(()))?;
            message.array(8, |_| Ok(()))?;
            message.i32(5000)
        },
    )
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn unescape_address(value: &str) -> io::Result<Vec<u8>> {
    let input = value.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'%' {
            output.push(input[index]);
            index += 1;
            continue;
        }
        if index + 2 >= input.len() {
            return Err(invalid("incomplete percent escape in D-Bus address"));
        }
        let high = hex_digit(input[index + 1]).ok_or_else(|| invalid("bad percent escape"))?;
        let low = hex_digit(input[index + 2]).ok_or_else(|| invalid("bad percent escape"))?;
        output.push((high << 4) | low);
        index += 3;
    }
    Ok(output)
}

fn connect_address(address: &str) -> io::Result<UnixStream> {
    let options = address
        .strip_prefix("unix:")
        .ok_or_else(|| invalid("unsupported D-Bus transport"))?;
    for option in options.split(',') {
        let Some((key, value)) = option.split_once('=') else {
            return Err(invalid("malformed D-Bus address option"));
        };
        let value = unescape_address(value)?;
        match key {
            "path" => return UnixStream::connect(Path::new(OsStr::from_bytes(&value))),
            "abstract" => {
                let address = SocketAddr::from_abstract_name(value)?;
                return UnixStream::connect_addr(&address);
            }
            _ => {}
        }
    }
    Err(invalid("unix D-Bus address has no path or abstract name"))
}

fn connect_bus(uid: u32) -> io::Result<UnixStream> {
    let mut last_error = None;
    if let Ok(addresses) = env::var("DBUS_SESSION_BUS_ADDRESS") {
        for address in addresses.split(';').filter(|value| !value.is_empty()) {
            match connect_address(address) {
                Ok(stream) => return configure_stream(stream),
                Err(error) => last_error = Some(error),
            }
        }
    }

    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        match UnixStream::connect(Path::new(&runtime).join("bus")) {
            Ok(stream) => return configure_stream(stream),
            Err(error) => last_error = Some(error),
        }
    }
    match UnixStream::connect(format!("/run/user/{uid}/bus")) {
        Ok(stream) => configure_stream(stream),
        Err(error) => Err(last_error.unwrap_or(error)),
    }
}

fn configure_stream(stream: UnixStream) -> io::Result<UnixStream> {
    stream.set_read_timeout(Some(METHOD_TIMEOUT))?;
    stream.set_write_timeout(Some(METHOD_TIMEOUT))?;
    Ok(stream)
}

fn hex(value: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for &byte in value {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    output
}

#[derive(Clone, Copy, Debug)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, bytes: &[u8]) -> u16 {
        let bytes: [u8; 2] = bytes.try_into().expect("two-byte slice");
        match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        }
    }

    fn u32(self, bytes: &[u8]) -> u32 {
        let bytes: [u8; 4] = bytes.try_into().expect("four-byte slice");
        match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        }
    }

    fn u64(self, bytes: &[u8]) -> u64 {
        let bytes: [u8; 8] = bytes.try_into().expect("eight-byte slice");
        match self {
            Self::Little => u64::from_le_bytes(bytes),
            Self::Big => u64::from_be_bytes(bytes),
        }
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
    endian: Endian,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], endian: Endian) -> Self {
        Self {
            bytes,
            position: 0,
            endian,
        }
    }

    fn align(&mut self, boundary: usize) -> io::Result<()> {
        let end = align(self.position, boundary)?;
        if end > self.bytes.len() || self.bytes[self.position..end].iter().any(|&byte| byte != 0) {
            return Err(invalid("invalid D-Bus alignment padding"));
        }
        self.position = end;
        Ok(())
    }

    fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| invalid("truncated D-Bus value"))?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        self.align(2)?;
        let endian = self.endian;
        Ok(endian.u16(self.take(2)?))
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.align(4)?;
        let endian = self.endian;
        Ok(endian.u32(self.take(4)?))
    }

    fn u64(&mut self) -> io::Result<u64> {
        self.align(8)?;
        let endian = self.endian;
        Ok(endian.u64(self.take(8)?))
    }

    fn text(&mut self, signature: bool) -> io::Result<String> {
        let length = if signature {
            self.byte()? as usize
        } else {
            self.u32()? as usize
        };
        let bytes = self.take(length)?;
        if bytes.contains(&0) || self.byte()? != 0 {
            return Err(invalid("invalid D-Bus string terminator"));
        }
        String::from_utf8(bytes.to_vec()).map_err(|_| invalid("non-UTF-8 D-Bus string"))
    }

    fn array_end(&mut self, element_alignment: usize) -> io::Result<usize> {
        let length = self.u32()? as usize;
        self.align(element_alignment)?;
        self.position
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| invalid("truncated D-Bus array"))
    }

    fn skip_value(&mut self, value_type: &ValueType) -> io::Result<()> {
        match value_type {
            ValueType::Byte => {
                self.byte()?;
            }
            ValueType::Bool => {
                let value = self.u32()?;
                if value > 1 {
                    return Err(invalid("invalid D-Bus boolean"));
                }
            }
            ValueType::I16 | ValueType::U16 => {
                self.u16()?;
            }
            ValueType::I32 | ValueType::U32 | ValueType::UnixFd => {
                self.u32()?;
            }
            ValueType::I64 | ValueType::U64 | ValueType::Double => {
                self.u64()?;
            }
            ValueType::String | ValueType::ObjectPath => {
                self.text(false)?;
            }
            ValueType::Signature => {
                self.text(true)?;
            }
            ValueType::Array(element) => {
                let end = self.array_end(element.alignment())?;
                while self.position < end {
                    let before = self.position;
                    self.skip_value(element)?;
                    if self.position <= before || self.position > end {
                        return Err(invalid("invalid D-Bus array element"));
                    }
                }
                if self.position != end {
                    return Err(invalid("D-Bus array length mismatch"));
                }
            }
            ValueType::Struct(fields) => {
                self.align(8)?;
                for field in fields {
                    self.skip_value(field)?;
                }
            }
            ValueType::DictEntry(key, value) => {
                self.align(8)?;
                self.skip_value(key)?;
                self.skip_value(value)?;
            }
            ValueType::Variant => {
                let signature = self.text(true)?;
                let contained = parse_single_type(&signature)?;
                self.skip_value(&contained)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ValueType {
    Byte,
    Bool,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Double,
    String,
    ObjectPath,
    Signature,
    UnixFd,
    Variant,
    Array(Box<ValueType>),
    Struct(Vec<ValueType>),
    DictEntry(Box<ValueType>, Box<ValueType>),
}

impl ValueType {
    fn alignment(&self) -> usize {
        match self {
            Self::Byte | Self::Signature | Self::Variant => 1,
            Self::I16 | Self::U16 => 2,
            Self::Bool
            | Self::I32
            | Self::U32
            | Self::String
            | Self::ObjectPath
            | Self::UnixFd
            | Self::Array(_) => 4,
            Self::I64
            | Self::U64
            | Self::Double
            | Self::Struct(_)
            | Self::DictEntry(_, _) => 8,
        }
    }
}

struct SignatureParser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> SignatureParser<'a> {
    fn parse_type(&mut self) -> io::Result<ValueType> {
        let byte = *self
            .bytes
            .get(self.position)
            .ok_or_else(|| invalid("truncated D-Bus signature"))?;
        self.position += 1;
        Ok(match byte {
            b'y' => ValueType::Byte,
            b'b' => ValueType::Bool,
            b'n' => ValueType::I16,
            b'q' => ValueType::U16,
            b'i' => ValueType::I32,
            b'u' => ValueType::U32,
            b'x' => ValueType::I64,
            b't' => ValueType::U64,
            b'd' => ValueType::Double,
            b's' => ValueType::String,
            b'o' => ValueType::ObjectPath,
            b'g' => ValueType::Signature,
            b'h' => ValueType::UnixFd,
            b'v' => ValueType::Variant,
            b'a' => ValueType::Array(Box::new(self.parse_type()?)),
            b'(' => {
                let mut fields = Vec::new();
                while self.bytes.get(self.position) != Some(&b')') {
                    fields.push(self.parse_type()?);
                }
                self.position += 1;
                if fields.is_empty() {
                    return Err(invalid("empty D-Bus struct signature"));
                }
                ValueType::Struct(fields)
            }
            b'{' => {
                let key = self.parse_type()?;
                let value = self.parse_type()?;
                if self.bytes.get(self.position) != Some(&b'}') {
                    return Err(invalid("unterminated D-Bus dictionary signature"));
                }
                self.position += 1;
                ValueType::DictEntry(Box::new(key), Box::new(value))
            }
            other => {
                return Err(invalid(format!(
                    "unsupported D-Bus type code {:?}",
                    other as char
                )));
            }
        })
    }
}

fn parse_single_type(signature: &str) -> io::Result<ValueType> {
    let mut parser = SignatureParser {
        bytes: signature.as_bytes(),
        position: 0,
    };
    let value_type = parser.parse_type()?;
    if parser.position != parser.bytes.len() {
        return Err(invalid("D-Bus variant signature contains multiple types"));
    }
    Ok(value_type)
}

#[derive(Debug)]
struct Frame {
    kind: u8,
    endian: Endian,
    path: Option<String>,
    interface: Option<String>,
    member: Option<String>,
    error_name: Option<String>,
    reply_serial: Option<u32>,
    #[cfg_attr(not(test), allow(dead_code))]
    destination: Option<String>,
    sender: Option<String>,
    signature: Option<String>,
    body: Vec<u8>,
}

fn frame_layout(fixed: &[u8; 16]) -> io::Result<(Endian, usize, usize, usize)> {
    let endian = match fixed[0] {
        b'l' => Endian::Little,
        b'B' => Endian::Big,
        _ => return Err(invalid("invalid D-Bus byte order")),
    };
    if fixed[3] != 1 {
        return Err(invalid("unsupported D-Bus protocol version"));
    }
    let body_len = endian.u32(&fixed[4..8]) as usize;
    let fields_len = endian.u32(&fixed[12..16]) as usize;
    let body_start = align(
        16usize
            .checked_add(fields_len)
            .ok_or_else(|| invalid("D-Bus header overflow"))?,
        8,
    )?;
    let total = body_start
        .checked_add(body_len)
        .ok_or_else(|| invalid("D-Bus message overflow"))?;
    if total > MAX_MESSAGE_SIZE {
        return Err(invalid("D-Bus reply exceeds 128 MiB"));
    }
    Ok((endian, fields_len, body_start, total))
}

fn parse_frame(fixed: &[u8; 16], rest: &[u8]) -> io::Result<Frame> {
    let (endian, fields_len, body_start, total) = frame_layout(fixed)?;
    if rest.len() != total - 16 || fields_len > rest.len() {
        return Err(invalid("truncated D-Bus frame"));
    }

    let mut fields = Decoder::new(&rest[..fields_len], endian);
    let mut path = None;
    let mut interface = None;
    let mut member = None;
    let mut error_name = None;
    let mut reply_serial = None;
    let mut destination = None;
    let mut sender = None;
    let mut signature = None;
    while fields.position < fields.bytes.len() {
        fields.align(8)?;
        if fields.position == fields.bytes.len() {
            break;
        }
        let code = fields.byte()?;
        let value_type = fields.text(true)?;
        match value_type.as_str() {
            "u" => {
                let value = fields.u32()?;
                if code == 5 {
                    reply_serial = Some(value);
                }
            }
            "s" | "o" => {
                let value = fields.text(false)?;
                match code {
                    1 => path = Some(value),
                    2 => interface = Some(value),
                    3 => member = Some(value),
                    4 => error_name = Some(value),
                    6 => destination = Some(value),
                    7 => sender = Some(value),
                    _ => {}
                }
            }
            "g" => {
                let value = fields.text(true)?;
                if code == 8 {
                    signature = Some(value);
                }
            }
            other => {
                return Err(invalid(format!(
                    "unsupported D-Bus header type {other}"
                )));
            }
        }
    }

    let body_offset = body_start - 16;
    Ok(Frame {
        kind: fixed[1],
        endian,
        path,
        interface,
        member,
        error_name,
        reply_serial,
        destination,
        sender,
        signature,
        body: rest[body_offset..].to_vec(),
    })
}

struct Connection {
    reader: BufReader<UnixStream>,
    frame_bytes: Vec<u8>,
    pending: VecDeque<Frame>,
}

fn authenticate(stream: UnixStream, uid: u32) -> io::Result<Connection> {
    let mut reader = BufReader::new(stream);
    let identity = hex(uid.to_string().as_bytes());
    let request = format!("\0AUTH EXTERNAL {identity}\r\n");
    reader.get_mut().write_all(request.as_bytes())?;

    let mut response = String::new();
    reader.by_ref().take(1025).read_line(&mut response)?;
    if response.starts_with("DATA") {
        write!(reader.get_mut(), "DATA {identity}\r\n")?;
        response.clear();
        reader.by_ref().take(1025).read_line(&mut response)?;
    }
    if response.len() > 1024 || !response.starts_with("OK ") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("D-Bus EXTERNAL authentication failed: {}", response.trim()),
        ));
    }
    Ok(Connection {
        reader,
        frame_bytes: Vec::with_capacity(512),
        pending: VecDeque::new(),
    })
}

impl Connection {
    fn begin(&mut self, hello: &[u8]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(7 + hello.len());
        bytes.extend_from_slice(b"BEGIN\r\n");
        bytes.extend_from_slice(hello);
        self.reader.get_mut().write_all(&bytes)
    }

    fn send(&mut self, message: &[u8]) -> io::Result<()> {
        self.reader.get_mut().write_all(message)
    }

    fn read_frame(&mut self) -> io::Result<Frame> {
        let mut fixed = [0; 16];
        self.reader.read_exact(&mut fixed)?;
        let (_, _, _, total) = frame_layout(&fixed)?;
        self.frame_bytes.resize(total - 16, 0);
        self.reader.read_exact(&mut self.frame_bytes)?;
        parse_frame(&fixed, &self.frame_bytes)
    }

    fn wait_for_reply(&mut self, serial: u32, operation: &str) -> io::Result<Frame> {
        if let Some(index) = self
            .pending
            .iter()
            .position(|frame| frame.reply_serial == Some(serial))
        {
            let frame = self.pending.remove(index).expect("pending frame index");
            return method_result(frame, operation);
        }

        loop {
            let frame = self.read_frame()?;
            if frame.reply_serial == Some(serial) {
                return method_result(frame, operation);
            }
            self.pending.push_back(frame);
        }
    }

    fn wait_for_response(&mut self, path: &str, sender: &str) -> io::Result<Frame> {
        if let Some(index) = self
            .pending
            .iter()
            .position(|frame| is_response(frame, path, sender))
        {
            return Ok(self.pending.remove(index).expect("pending frame index"));
        }

        loop {
            let frame = self.read_frame()?;
            if is_response(&frame, path, sender) {
                return Ok(frame);
            }
        }
    }

    fn allow_interaction_wait(&self) -> io::Result<()> {
        self.reader.get_ref().set_read_timeout(None)
    }
}

fn method_result(frame: Frame, operation: &str) -> io::Result<Frame> {
    match frame.kind {
        METHOD_RETURN => Ok(frame),
        ERROR => {
            let name = frame.error_name.as_deref().unwrap_or("unknown error");
            let text = Decoder::new(&frame.body, frame.endian)
                .text(false)
                .unwrap_or_default();
            Err(io::Error::other(format!(
                "D-Bus {operation} failed: {name}: {text}"
            )))
        }
        _ => Err(invalid("reply serial appeared on a non-reply message")),
    }
}

fn is_response(frame: &Frame, path: &str, sender: &str) -> bool {
    frame.kind == SIGNAL
        && frame.path.as_deref() == Some(path)
        && frame.interface.as_deref() == Some(REQUEST_INTERFACE)
        && frame.member.as_deref() == Some("Response")
        && frame.sender.as_deref() == Some(sender)
}

fn connect_session() -> io::Result<(Connection, String)> {
    // SAFETY: libc::geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut connection = authenticate(connect_bus(uid)?, uid)?;
    connection.begin(&hello_message(1)?)?;
    let hello = connection.wait_for_reply(1, "Hello")?;
    let unique_name = decode_single_text(&hello, false)?;
    if !unique_name.starts_with(':') {
        return Err(invalid("D-Bus Hello returned an invalid unique name"));
    }
    Ok((connection, unique_name))
}

fn decode_single_text(frame: &Frame, signature: bool) -> io::Result<String> {
    let mut decoder = Decoder::new(&frame.body, frame.endian);
    let value = decoder.text(signature)?;
    if decoder.position != decoder.bytes.len() {
        return Err(invalid("unexpected trailing D-Bus reply data"));
    }
    Ok(value)
}

fn decode_single_u32(frame: &Frame) -> io::Result<u32> {
    let mut decoder = Decoder::new(&frame.body, frame.endian);
    let value = decoder.u32()?;
    if decoder.position != decoder.bytes.len() {
        return Err(invalid("unexpected trailing D-Bus reply data"));
    }
    Ok(value)
}

fn request_token() -> io::Result<String> {
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(format!("dbus_message_{}", hex(&random)))
}

fn expected_request_path(unique_name: &str, token: &str) -> io::Result<String> {
    let sender = unique_name
        .strip_prefix(':')
        .ok_or_else(|| invalid("invalid D-Bus unique name"))?
        .replace('.', "_");
    if sender.is_empty()
        || token.is_empty()
        || !sender.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(invalid("invalid portal request path element"));
    }
    Ok(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{token}"
    ))
}

fn file_chooser(request: FileChooserRequest) -> io::Result<FileChooserResponse> {
    let (mut connection, unique_name) = connect_session()?;
    let mut serial = 2;

    connection.send(&start_service_message(serial, PORTAL_DESTINATION)?)?;
    let started = connection.wait_for_reply(serial, "StartServiceByName")?;
    match decode_single_u32(&started)? {
        1 | 2 => {}
        value => return Err(invalid(format!("unexpected D-Bus service start result {value}"))),
    }
    serial += 1;

    connection.send(&get_name_owner_message(serial, PORTAL_DESTINATION)?)?;
    let owner = connection.wait_for_reply(serial, "GetNameOwner")?;
    let owner = decode_single_text(&owner, false)?;
    if !owner.starts_with(':') {
        return Err(invalid("portal has an invalid D-Bus owner"));
    }
    serial += 1;

    let match_rule = concat!(
        "type='signal',",
        "sender='org.freedesktop.portal.Desktop',",
        "interface='org.freedesktop.portal.Request',",
        "member='Response'"
    );
    connection.send(&add_match_message(serial, match_rule)?)?;
    connection.wait_for_reply(serial, "AddMatch")?;
    serial += 1;

    let token = request_token()?;
    let expected_path = expected_request_path(&unique_name, &token)?;
    connection.send(&file_chooser_message(serial, &request, &token)?)?;
    let reply = connection.wait_for_reply(serial, request.member())?;
    let actual_path = decode_single_text(&reply, false)?;
    if !actual_path.starts_with("/org/freedesktop/portal/desktop/request/") {
        return Err(invalid("portal returned an invalid request object path"));
    }
    if actual_path != expected_path {
        // The broad Response match supports pre-0.9 portals. Keep accepting the
        // returned handle, but never accept a signal for any other object.
        log_path_mismatch(&expected_path, &actual_path);
    }

    connection.allow_interaction_wait()?;
    let response = connection.wait_for_response(&actual_path, &owner)?;
    decode_file_chooser_response(&response)
}

fn log_path_mismatch(expected: &str, actual: &str) {
    eprintln!("portal returned legacy request path {actual}; expected {expected}");
}

fn decode_file_chooser_response(frame: &Frame) -> io::Result<FileChooserResponse> {
    if frame.signature.as_deref() != Some("ua{sv}") {
        return Err(invalid("portal Response has an unexpected signature"));
    }

    let mut decoder = Decoder::new(&frame.body, frame.endian);
    let response = decoder.u32()?;
    let dictionary_end = decoder.array_end(8)?;
    let mut uris = None;
    while decoder.position < dictionary_end {
        decoder.align(8)?;
        let key = decoder.text(false)?;
        let signature = decoder.text(true)?;
        let value_type = parse_single_type(&signature)?;
        if key == "uris" {
            if value_type != ValueType::Array(Box::new(ValueType::String)) {
                return Err(invalid("portal uris result has an unexpected type"));
            }
            uris = Some(decode_string_array(&mut decoder)?);
        } else {
            decoder.skip_value(&value_type)?;
        }
        if decoder.position > dictionary_end {
            return Err(invalid("portal result exceeds its dictionary"));
        }
    }
    if decoder.position != dictionary_end || dictionary_end != decoder.bytes.len() {
        return Err(invalid("portal Response has trailing or truncated data"));
    }

    match response {
        0 => {
            let uris = uris.ok_or_else(|| invalid("successful portal response has no uris"))?;
            let paths = uris
                .iter()
                .map(|uri| file_uri_to_path(uri))
                .collect::<io::Result<Vec<_>>>()?;
            if paths.is_empty() {
                return Err(invalid("successful portal response has an empty uri list"));
            }
            Ok(FileChooserResponse::Selected(paths))
        }
        1 => Ok(FileChooserResponse::Cancelled),
        2 => Ok(FileChooserResponse::Other),
        value => Err(invalid(format!(
            "portal returned unknown response code {value}"
        ))),
    }
}

fn decode_string_array(decoder: &mut Decoder<'_>) -> io::Result<Vec<String>> {
    let end = decoder.array_end(4)?;
    let mut values = Vec::new();
    while decoder.position < end {
        values.push(decoder.text(false)?);
        if decoder.position > end {
            return Err(invalid("D-Bus string exceeds array boundary"));
        }
    }
    if decoder.position != end {
        return Err(invalid("D-Bus string array length mismatch"));
    }
    Ok(values)
}

fn file_uri_to_path(uri: &str) -> io::Result<PathBuf> {
    let encoded = uri
        .strip_prefix("file://")
        .ok_or_else(|| invalid("portal returned a non-file URI"))?;
    let encoded = if encoded.starts_with('/') {
        encoded
    } else {
        encoded
            .strip_prefix("localhost")
            .ok_or_else(|| invalid("portal returned a remote file URI"))?
    };
    if !encoded.starts_with('/') {
        return Err(invalid("portal returned a relative file URI"));
    }

    let encoded = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] == b'%' {
            let high = encoded
                .get(index + 1)
                .and_then(|byte| hex_digit(*byte))
                .ok_or_else(|| invalid("malformed percent escape in file URI"))?;
            let low = encoded
                .get(index + 2)
                .and_then(|byte| hex_digit(*byte))
                .ok_or_else(|| invalid("malformed percent escape in file URI"))?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(encoded[index]);
            index += 1;
        }
    }
    if decoded.contains(&0) {
        return Err(invalid("portal returned a file URI containing NUL"));
    }
    Ok(PathBuf::from(OsString::from_vec(decoded)))
}

pub fn send_notification(title: &str, body: &str) -> io::Result<()> {
    let (mut connection, _) = connect_session()?;
    connection.send(&notification_message(2, title, body)?)?;
    connection.wait_for_reply(2, "Notify")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_start(message: &[u8]) -> usize {
        let fields_len = u32::from_le_bytes(message[12..16].try_into().unwrap()) as usize;
        align(16 + fields_len, 8).unwrap()
    }

    fn parsed_message(message: &[u8]) -> Frame {
        let fixed: &[u8; 16] = message[..16].try_into().unwrap();
        parse_frame(fixed, &message[16..]).unwrap()
    }

    fn response_frame(
        status: u32,
        entries: impl FnOnce(&mut Encoder) -> io::Result<()>,
    ) -> Frame {
        let mut body = Encoder::new();
        body.u32(status).unwrap();
        body.dict(entries).unwrap();
        Frame {
            kind: SIGNAL,
            endian: Endian::Little,
            path: Some("/org/freedesktop/portal/desktop/request/1_2/token".into()),
            interface: Some(REQUEST_INTERFACE.into()),
            member: Some("Response".into()),
            error_name: None,
            reply_serial: None,
            destination: Some(":1.2".into()),
            sender: Some(":1.3".into()),
            signature: Some("ua{sv}".into()),
            body: body.bytes,
        }
    }

    fn encode_string_array(message: &mut Encoder, values: &[&str]) -> io::Result<()> {
        message.array(4, |message| {
            for value in values {
                message.string(value)?;
            }
            Ok(())
        })
    }

    #[test]
    fn hello_is_a_reply_expected_method_call() {
        let message = hello_message(1).unwrap();
        assert_eq!(&message[0..4], b"l\x01\x00\x01");
        assert_eq!(u32::from_le_bytes(message[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(message[8..12].try_into().unwrap()), 1);
        assert!(message.windows(5).any(|part| part == b"Hello"));
    }

    #[test]
    fn notification_body_length_and_signature_are_correct() {
        let message = notification_message(2, "title", "body").unwrap();
        let body_len = u32::from_le_bytes(message[4..8].try_into().unwrap()) as usize;
        assert_eq!(message.len(), body_start(&message) + body_len);
        assert_eq!(message[2], 0);
        assert!(message.windows(13).any(|part| part == b"susssasa{sv}i"));
    }

    #[test]
    fn file_chooser_call_uses_portal_destination_and_signature() {
        let request = FileChooserRequest::Open(OpenFileOptions {
            multiple: true,
            ..OpenFileOptions::default()
        });
        let frame = parsed_message(&file_chooser_message(5, &request, "token_1").unwrap());
        assert_eq!(frame.kind, METHOD_CALL);
        assert_eq!(frame.path.as_deref(), Some(PORTAL_PATH));
        assert_eq!(frame.interface.as_deref(), Some(FILE_CHOOSER_INTERFACE));
        assert_eq!(frame.member.as_deref(), Some("OpenFile"));
        assert_eq!(frame.destination.as_deref(), Some(PORTAL_DESTINATION));
        assert_eq!(frame.signature.as_deref(), Some("ssa{sv}"));
    }

    #[test]
    fn open_file_options_encode_typed_variant_dictionary() {
        let request = FileChooserRequest::Open(OpenFileOptions {
            title: "Choose media".into(),
            accept_label: Some("Upload".into()),
            multiple: true,
            directory: false,
            current_folder: Some(PathBuf::from("/tmp/media")),
            parent_window: String::new(),
        });
        let frame = parsed_message(&file_chooser_message(5, &request, "token_1").unwrap());
        let mut decoder = Decoder::new(&frame.body, frame.endian);
        assert_eq!(decoder.text(false).unwrap(), "");
        assert_eq!(decoder.text(false).unwrap(), "Choose media");
        let end = decoder.array_end(8).unwrap();
        let mut seen = Vec::new();
        while decoder.position < end {
            decoder.align(8).unwrap();
            let key = decoder.text(false).unwrap();
            let signature = decoder.text(true).unwrap();
            let value_type = parse_single_type(&signature).unwrap();
            decoder.skip_value(&value_type).unwrap();
            seen.push((key, signature));
        }
        assert_eq!(
            seen,
            [
                ("handle_token".into(), "s".into()),
                ("accept_label".into(), "s".into()),
                ("modal".into(), "b".into()),
                ("multiple".into(), "b".into()),
                ("directory".into(), "b".into()),
                ("current_folder".into(), "ay".into()),
            ]
        );
    }

    #[test]
    fn decodes_success_response_with_unrelated_values_before_uris() {
        let frame = response_frame(0, |message| {
            message.option("choices", "a(ss)", |message| {
                message.array(8, |message| {
                    message.align(8)?;
                    message.string("encoding")?;
                    message.string("utf8")
                })
            })?;
            message.option("uris", "as", |message| {
                encode_string_array(
                    message,
                    &["file:///tmp/hello%20world.txt", "file:///tmp/%ff.bin"],
                )
            })
        });
        assert_eq!(
            decode_file_chooser_response(&frame).unwrap(),
            FileChooserResponse::Selected(vec![
                PathBuf::from("/tmp/hello world.txt"),
                PathBuf::from(OsString::from_vec(b"/tmp/\xff.bin".to_vec())),
            ])
        );
    }

    #[test]
    fn decodes_cancel_and_other_responses_without_uris() {
        assert_eq!(
            decode_file_chooser_response(&response_frame(1, |_| Ok(()))).unwrap(),
            FileChooserResponse::Cancelled
        );
        assert_eq!(
            decode_file_chooser_response(&response_frame(2, |_| Ok(()))).unwrap(),
            FileChooserResponse::Other
        );
    }

    #[test]
    fn rejects_success_without_uris_or_with_remote_uri() {
        assert!(decode_file_chooser_response(&response_frame(0, |_| Ok(()))).is_err());
        let remote = response_frame(0, |message| {
            message.option("uris", "as", |message| {
                encode_string_array(message, &["file://example.com/tmp/file"])
            })
        });
        assert!(decode_file_chooser_response(&remote).is_err());
    }

    #[test]
    fn expected_path_sanitizes_the_unique_bus_name() {
        assert_eq!(
            expected_request_path(":1.42", "dbus_message_abcd").unwrap(),
            "/org/freedesktop/portal/desktop/request/1_42/dbus_message_abcd"
        );
        assert!(expected_request_path("org.example.NotUnique", "token").is_err());
        assert!(expected_request_path(":1.2", "bad-token").is_err());
    }

    #[test]
    fn address_percent_decoding_is_strict() {
        assert_eq!(unescape_address("/run/a%20b/bus").unwrap(), b"/run/a b/bus");
        assert!(unescape_address("bad%2").is_err());
        assert!(unescape_address("bad%xx").is_err());
    }

    #[test]
    fn strings_reject_nul() {
        assert!(notification_message(2, "bad\0title", "body").is_err());
    }

    fn method_return(byte_order: u8, reply_serial: u32) -> Vec<u8> {
        let encode = |value: u32| match byte_order {
            b'l' => value.to_le_bytes(),
            b'B' => value.to_be_bytes(),
            _ => unreachable!(),
        };
        let mut message = vec![byte_order, METHOD_RETURN, 0, 1];
        message.extend_from_slice(&encode(0));
        message.extend_from_slice(&encode(99));
        message.extend_from_slice(&encode(8));
        message.extend_from_slice(&[5, 1, b'u', 0]);
        message.extend_from_slice(&encode(reply_serial));
        message
    }

    #[test]
    fn replies_support_both_dbus_byte_orders() {
        for byte_order in [b'l', b'B'] {
            let message = method_return(byte_order, 42);
            let frame = parsed_message(&message);
            assert_eq!(frame.kind, METHOD_RETURN);
            assert_eq!(frame.reply_serial, Some(42));
        }
    }

    #[test]
    fn recursive_signature_parser_skips_portal_result_shapes() {
        assert_eq!(
            parse_single_type("a(ss)").unwrap(),
            ValueType::Array(Box::new(ValueType::Struct(vec![
                ValueType::String,
                ValueType::String,
            ])))
        );
        assert_eq!(
            parse_single_type("(sa(us))").unwrap(),
            ValueType::Struct(vec![
                ValueType::String,
                ValueType::Array(Box::new(ValueType::Struct(vec![
                    ValueType::U32,
                    ValueType::String,
                ]))),
            ])
        );
        assert!(parse_single_type("ss").is_err());
        assert!(parse_single_type("(").is_err());
    }
}
