use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub(crate) fn encode_message<M: Serialize>(message: &M, max_size: usize) -> io::Result<Vec<u8>> {
    let payload = bincode::serde::encode_to_vec(message, bincode::config::standard())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.len() > max_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "encoded frame size {} exceeds maximum {max_size}",
                payload.len()
            ),
        ));
    }
    let len = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "message exceeds u32 frame size")
    })?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub(crate) fn write_message<W: Write, M: Serialize>(writer: &mut W, message: &M) -> io::Result<()> {
    writer.write_all(&encode_message(message, u32::MAX as usize)?)?;
    writer.flush()
}

pub(crate) fn read_message<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
    max_size: usize,
) -> io::Result<M> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame size {len} exceeds maximum {max_size}"),
        ));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    let (message, consumed) =
        bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if consumed != len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("decoded {consumed} bytes but payload length was {len}"),
        ));
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unexpected_eof_for_truncated_length_prefix() {
        let mut frame = [1, 0, 0].as_slice();
        let error = read_message::<_, u8>(&mut frame, 8).expect_err("reject truncated prefix");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn preserves_unexpected_eof_for_truncated_payload() {
        let mut bytes = 2_u32.to_le_bytes().to_vec();
        bytes.push(1);
        let error =
            read_message::<_, u8>(&mut bytes.as_slice(), 8).expect_err("reject truncated payload");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn rejects_trailing_payload_bytes() {
        let mut bytes = 2_u32.to_le_bytes().to_vec();
        bytes.extend([1, 2]);
        let error =
            read_message::<_, u8>(&mut bytes.as_slice(), 8).expect_err("reject trailing bytes");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn accepts_exact_maximum_and_rejects_one_smaller() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &1_u8).expect("encode frame");
        let payload_len = bytes.len() - 4;

        assert_eq!(
            read_message::<_, u8>(&mut bytes.as_slice(), payload_len)
                .expect("accept exact maximum"),
            1
        );
        let error = read_message::<_, u8>(&mut bytes.as_slice(), payload_len - 1)
            .expect_err("reject one-smaller maximum");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_oversized_frame_before_allocating_payload() {
        let bytes = 9_u32.to_le_bytes();
        let mut frame = bytes.as_slice();
        let error = read_message::<_, u8>(&mut frame, 8).expect_err("reject oversized frame");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn encode_rejects_payload_over_bridge_limit() {
        let error = encode_message(&vec![0_u8; 9], 8).expect_err("reject oversized payload");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
