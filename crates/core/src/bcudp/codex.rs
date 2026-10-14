//! Handles sending and recieving messages as complete packets
//!
//! BcUdpCodex is used with a `[tokio_util::codec::Framed]` to form complete packets
//!
use crate::bcudp::model::*;
use crate::{Error, Result};
use bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

pub(crate) struct BcUdpCodex {}

impl BcUdpCodex {
    pub(crate) fn new() -> Self {
        Self {}
    }
}

impl Encoder<BcUdp> for BcUdpCodex {
    type Error = Error;

    fn encode(&mut self, item: BcUdp, dst: &mut BytesMut) -> Result<()> {
        log::trace!("Encoding: {item:?}");
        let buf: Vec<u8> = Default::default();
        let buf = item.serialize(buf)?;
        dst.extend_from_slice(buf.as_slice());
        log::trace!("  Encoding: Done: {}", buf.len());
        Ok(())
    }
}

impl Decoder for BcUdpCodex {
    type Item = BcUdp;
    type Error = Error;

    // R2C_DISC and D2C_DISC are returned as normal packets. Only the consumer knows
    // whether the sender's disconnect ends anything: a relay dropping an abandoned
    // relay attempt must not end a working direct session.
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        log::trace!("Decoding:");
        if src.is_empty() {
            return Ok(None);
        }
        match BcUdp::deserialize(src) {
            Ok(bc) => {
                log::trace!("   Decoding: Ok");
                Ok(Some(bc))
            }
            Err(Error::NomIncomplete(_)) => {
                log::trace!("   Decoding: Incomplete: {:0X?}", src);
                Ok(None)
            }
            Err(e) => {
                log::trace!("   Decoding: Err");
                // `UdpFramed` keeps the read buffer after an error and decodes it again
                // on the next poll, so drop the bad datagram or it fails forever
                src.clear();
                Err(e)
            }
        }
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        match self.decode(src)? {
            Some(frame) => Ok(Some(frame)),
            None if src.is_empty() => Ok(None),
            None => {
                // `UdpFramed` hands over one whole datagram at a time, so an incomplete
                // packet is a truncated datagram that no later read will complete
                let len = src.len();
                src.clear();
                Err(Error::NomError(format!(
                    "Truncated UDP packet: {} bytes could not be decoded",
                    len
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bcudp::xml::*;
    use assert_matches::assert_matches;

    fn encode(bcudp: BcUdp) -> BytesMut {
        let mut buf = BytesMut::new();
        BcUdpCodex::new().encode(bcudp, &mut buf).unwrap();
        buf
    }

    #[test]
    // A relay or camera disconnect is a packet, not an error
    fn test_disc_decodes_as_packet() {
        let mut buf = encode(BcUdp::Discovery(UdpDiscovery {
            tid: 42,
            payload: UdpXml::R2cDisc(R2cDisc { sid: 1234 }),
        }));
        assert_matches!(
            BcUdpCodex::new().decode_eof(&mut buf),
            Ok(Some(BcUdp::Discovery(UdpDiscovery {
                tid: 42,
                payload: UdpXml::R2cDisc(R2cDisc { sid: 1234 }),
            })))
        );

        let mut buf = encode(BcUdp::Discovery(UdpDiscovery {
            tid: 7,
            payload: UdpXml::D2cDisc(D2cDisc { cid: 1, did: 2 }),
        }));
        assert_matches!(
            BcUdpCodex::new().decode_eof(&mut buf),
            Ok(Some(BcUdp::Discovery(UdpDiscovery {
                payload: UdpXml::D2cDisc(D2cDisc { cid: 1, did: 2 }),
                ..
            })))
        );
    }

    #[test]
    // A bad datagram must not stay in the buffer to fail again on the next poll
    fn test_bad_packet_clears_buffer() {
        let mut codec = BcUdpCodex::new();

        let mut buf = BytesMut::from(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0x01][..]);
        assert!(codec.decode_eof(&mut buf).is_err());
        assert!(buf.is_empty());

        // A valid header whose payload is cut short
        let mut buf = encode(BcUdp::Discovery(UdpDiscovery {
            tid: 42,
            payload: UdpXml::R2cDisc(R2cDisc { sid: 1234 }),
        }));
        buf.truncate(buf.len() - 4);
        assert!(codec.decode_eof(&mut buf).is_err());
        assert!(buf.is_empty());
    }
}
