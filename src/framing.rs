use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub(crate) fn write_message<W: Write, M: Serialize>(writer: &mut W, message: &M) -> io::Result<()> {
    let payload = bincode::serde::encode_to_vec(message, bincode::config::standard())
        .map_err(io::Error::other)?;
    let len = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "message exceeds u32 frame size")
    })?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)?;
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
            .map_err(io::Error::other)?;
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
    fn rejects_oversized_frame_before_allocating_payload() {
        let bytes = 9_u32.to_le_bytes();
        let mut frame = bytes.as_slice();
        let error = read_message::<_, u8>(&mut frame, 8).expect_err("reject oversized frame");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
