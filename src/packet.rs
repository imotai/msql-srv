use byteorder::{ByteOrder, LittleEndian};
use rustls::{ServerConfig, ServerConnection};
use std::io;
use std::io::prelude::*;

const U24_MAX: usize = 16_777_215;

pub struct PacketConn<RW: Read + Write> {
    rw: SwitchableConn<RW>,

    // read variables
    bytes: Vec<u8>,
    start: usize,
    remaining: usize,

    // write variables
    to_write: Vec<u8>,
    seq: u8,
}

impl<W: Read + Write> Write for PacketConn<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use std::cmp::min;
        let left = min(buf.len(), U24_MAX - self.to_write.len());
        self.to_write.extend(&buf[..left]);

        if self.to_write.len() == U24_MAX {
            self.end_packet()?;
        }
        Ok(left)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.maybe_end_packet()?;
        self.rw.flush()
    }
}

impl<RW: Read + Write> PacketConn<RW> {
    pub fn new(rw: RW) -> Self {
        PacketConn {
            bytes: Vec::new(),
            start: 0,
            remaining: 0,

            to_write: vec![0, 0, 0, 0],
            seq: 0,
            rw: SwitchableConn::new(rw),
        }
    }
}

impl<W: Read + Write> PacketConn<W> {
    fn maybe_end_packet(&mut self) -> io::Result<()> {
        let len = self.to_write.len() - 4;
        if len != 0 {
            LittleEndian::write_u24(&mut self.to_write[0..3], len as u32);
            self.to_write[3] = self.seq;
            self.seq = self.seq.wrapping_add(1);

            self.rw.write_all(&self.to_write[..])?;
            self.to_write.truncate(4); // back to just header
        }
        Ok(())
    }

    pub fn end_packet(&mut self) -> io::Result<()> {
        self.maybe_end_packet()
    }

    pub fn switch_to_tls(&mut self, config: Arc<ServerConfig>) -> io::Result<()> {
        assert_eq!(self.remaining(), 0); // otherwise we've read ahead into the TLS handshake and will be in trouble.

        self.rw.switch_to_tls(config)
    }
}

impl<W: Read + Write> PacketConn<W> {
    pub fn set_seq(&mut self, seq: u8) {
        self.seq = seq;
    }
}

impl<R: Read + Write> PacketConn<R> {
    pub fn next(&mut self) -> io::Result<Option<(u8, Packet)>> {
        self.start = self.bytes.len() - self.remaining;

        loop {
            if self.remaining != 0 {
                let bytes = {
                    // NOTE: this is all sorts of unfortunate. what we really want to do is to give
                    // &self.bytes[self.start..] to `packet()`, and the lifetimes should all work
                    // out. however, without NLL, borrowck doesn't realize that self.bytes is no
                    // longer borrowed after the match, and so can be mutated.
                    let bytes = &self.bytes[self.start..];
                    unsafe { ::std::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) }
                };
                match packet(bytes) {
                    Ok((rest, p)) => {
                        self.remaining = rest.len();
                        return Ok(Some(p));
                    }
                    Err(nom::Err::Incomplete(_)) | Err(nom::Err::Error(_)) => {}
                    Err(nom::Err::Failure(ctx)) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("{:?}", ctx),
                        ))
                    }
                }
            }

            // we need to read some more
            self.bytes.drain(0..self.start);
            self.start = 0;
            let end = self.bytes.len();
            self.bytes.resize(std::cmp::max(4096, end * 2), 0);
            let read = {
                let mut buf = &mut self.bytes[end..];
                self.rw.read(&mut buf)?
            };
            self.bytes.truncate(end + read);
            self.remaining = self.bytes.len();

            if read == 0 {
                if self.bytes.is_empty() {
                    return Ok(None);
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("{} unhandled bytes", self.bytes.len()),
                    ));
                }
            }
        }
    }

    pub fn remaining(&self) -> usize {
        self.remaining
    }
}

pub fn fullpacket(i: &[u8]) -> nom::IResult<&[u8], (u8, &[u8])> {
    let (i, _) = nom::bytes::complete::tag(&[0xff, 0xff, 0xff])(i)?;
    let (i, seq) = nom::bytes::complete::take(1u8)(i)?;
    let (i, bytes) = nom::bytes::complete::take(U24_MAX)(i)?;
    Ok((i, (seq[0], bytes)))
}

pub fn onepacket(i: &[u8]) -> nom::IResult<&[u8], (u8, &[u8])> {
    let (i, length) = nom::number::complete::le_u24(i)?;
    let (i, seq) = nom::bytes::complete::take(1u8)(i)?;
    let (i, bytes) = nom::bytes::complete::take(length)(i)?;
    Ok((i, (seq[0], bytes)))
}

// Clone because of https://github.com/Geal/nom/issues/1008
#[derive(Clone)]
pub struct Packet(Vec<u8>);

impl Packet {
    fn extend(&mut self, bytes: &[u8]) {
        self.0.extend(bytes);
    }
}

impl AsRef<[u8]> for Packet {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

use std::ops::Deref;
use std::sync::Arc;

use crate::tls;
impl Deref for Packet {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

fn packet(i: &[u8]) -> nom::IResult<&[u8], (u8, Packet)> {
    nom::combinator::map(
        nom::sequence::pair(
            nom::multi::fold_many0(
                fullpacket,
                (0, None),
                |(seq, pkt): (_, Option<Packet>), (nseq, p)| {
                    let pkt = if let Some(mut pkt) = pkt {
                        assert_eq!(nseq, seq + 1);
                        pkt.extend(p);
                        Some(pkt)
                    } else {
                        Some(Packet(Vec::from(p)))
                    };
                    (nseq, pkt)
                },
            ),
            onepacket,
        ),
        move |(full, last)| {
            let seq = last.0;
            let pkt = if let Some(mut pkt) = full.1 {
                assert_eq!(last.0, full.0 + 1);
                pkt.extend(last.1);
                pkt
            } else {
                Packet(Vec::from(last.1))
            };
            (seq, pkt)
        },
    )(i)
}

pub(crate) struct SwitchableConn<T: Read + Write>(Option<EitherConn<T>>);

pub(crate) enum EitherConn<T: Read + Write> {
    Plain(T),
    TLS(rustls::StreamOwned<ServerConnection, T>),
}

impl<T: Read + Write> Read for SwitchableConn<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.0.as_mut().unwrap() {
            EitherConn::Plain(p) => p.read(buf),
            EitherConn::TLS(t) => t.read(buf),
        }
    }
}

impl<T: Read + Write> Write for SwitchableConn<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.0.as_mut().unwrap() {
            EitherConn::Plain(p) => p.write(buf),
            EitherConn::TLS(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.0.as_mut().unwrap() {
            EitherConn::Plain(p) => p.flush(),
            EitherConn::TLS(t) => t.flush(),
        }
    }
}

impl<T: Read + Write> SwitchableConn<T> {
    pub fn new(rw: T) -> SwitchableConn<T> {
        SwitchableConn(Some(EitherConn::Plain(rw)))
    }

    pub fn switch_to_tls(&mut self, config: Arc<ServerConfig>) -> io::Result<()> {
        let replacement = match self.0.take() {
            Some(EitherConn::Plain(plain)) => {
                Ok(EitherConn::TLS(tls::create_stream(plain, config)?))
            }
            Some(EitherConn::TLS(_)) => Err(io::Error::new(
                io::ErrorKind::Other,
                "tls variant found when plain was expected",
            )),
            None => unreachable!(),
        }?;

        self.0 = Some(replacement);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_one_ping() {
        assert_eq!(
            onepacket(&[0x01, 0, 0, 0, 0x10]).unwrap().1,
            (0, &[0x10][..])
        );
    }

    #[test]
    fn test_ping() {
        let p = packet(&[0x01, 0, 0, 0, 0x10]).unwrap().1;
        assert_eq!(p.0, 0);
        assert_eq!(&*p.1, &[0x10][..]);
    }

    #[test]
    fn test_long_exact() {
        let mut data = Vec::new();
        data.push(0xff);
        data.push(0xff);
        data.push(0xff);
        data.push(0);
        data.extend(&[0; U24_MAX][..]);
        data.push(0x00);
        data.push(0x00);
        data.push(0x00);
        data.push(1);

        let (rest, p) = packet(&data[..]).unwrap();
        assert!(rest.is_empty());
        assert_eq!(p.0, 1);
        assert_eq!(p.1.len(), U24_MAX);
        assert_eq!(&*p.1, &[0; U24_MAX][..]);
    }

    #[test]
    fn test_long_more() {
        let mut data = Vec::new();
        data.push(0xff);
        data.push(0xff);
        data.push(0xff);
        data.push(0);
        data.extend(&[0; U24_MAX][..]);
        data.push(0x01);
        data.push(0x00);
        data.push(0x00);
        data.push(1);
        data.push(0x10);

        let (rest, p) = packet(&data[..]).unwrap();
        assert!(rest.is_empty());
        assert_eq!(p.0, 1);
        assert_eq!(p.1.len(), U24_MAX + 1);
        assert_eq!(&p.1[..U24_MAX], &[0; U24_MAX][..]);
        assert_eq!(&p.1[U24_MAX..], &[0x10]);
    }
}
