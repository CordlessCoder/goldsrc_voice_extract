use steam_audio_codec::{Packet, SteamVoiceData};
use opus::{Channels, Decoder};
use rsmpeg::{
    ffi::{
        AV_SAMPLE_FMT_S16,
        AV_SAMPLE_FMT_FLT
    }
};

use crate::SAMPLE_RATE;

const FRAME_SIZE: usize = 960;

pub struct SteamVoiceDecoder {
    decoder: Decoder,
    seq: u16,
    decode_fn: Box<dyn Fn(&mut Decoder, &[u8], &mut [u8]) -> Result<usize, Box<dyn std::error::Error>>>,
}

fn read_bytes<const N: usize>(data: &[u8]) -> Result<([u8; N], &[u8]), Box<dyn std::error::Error>> {
    if data.len() < N {
        Err("InsufficientData".into())
    } else {
        let (result, rest) = data.split_at(N);
        Ok((result.try_into().unwrap(), rest))
    }
}

fn read_u16(data: &[u8]) -> Result<(u16, &[u8]), Box<dyn std::error::Error>> {
    let (bytes, data) = read_bytes(data)?;
    Ok((u16::from_le_bytes(bytes), data))
}

impl SteamVoiceDecoder {
    pub fn new(sample_format: i32) -> Result<Self, Box<dyn std::error::Error>> {
        let decoder = Decoder::new(SAMPLE_RATE as u32, Channels::Mono)?;
        let decode_fn: Box<dyn Fn(&mut Decoder, &[u8], &mut [u8]) -> Result<usize, Box<dyn std::error::Error>>> = match sample_format {
            AV_SAMPLE_FMT_S16 => Box::new(|decoder, input, output| {
                let output_length = if input.len() == 0 { FRAME_SIZE } else { output.len() } / std::mem::size_of::<i16>();
                let out = unsafe {
                    std::slice::from_raw_parts_mut(output.as_mut_ptr() as *mut i16, output_length)
                };
                let n = decoder.decode(input, out, false)?;
                Ok(n * std::mem::size_of::<i16>())
            }),
            AV_SAMPLE_FMT_FLT => Box::new(|decoder, input, output| {
                let output_length = if input.len() == 0 { FRAME_SIZE } else { output.len() } / std::mem::size_of::<f32>();
                let out = unsafe {
                    std::slice::from_raw_parts_mut(output.as_mut_ptr() as *mut f32, output_length)
                };
                let n = decoder.decode_float(input, out, false)?;
                Ok(n * std::mem::size_of::<f32>())
            }),
            _ => panic!("decoder created with sample format that we didn't account for!")
        };

        Ok(Self {
            decoder,
            seq: 0,
            decode_fn
        })
    }

    pub fn decode(
        &mut self,
        voice_data: SteamVoiceData,
        output_buffer: &mut [u8],
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let mut total = 0;
        for packet in voice_data.packets() {
            let packet = packet.expect("Coudln't read packet??");
            match packet {
                Packet::SampleRate(rate) => {
                    if rate != SAMPLE_RATE as u16 {
                        panic!("Sample rate was something other than {SAMPLE_RATE}!");
                    }
                }
                Packet::OpusPlc(opus) => {
                    let size = self.decode_opus(opus.data, &mut output_buffer[total..])?;
                    total += size;
                    if total >= output_buffer.len() {
                        return Err("InsufficientOutputBuffer".into());
                    }
                }
                Packet::Silence(silence) => {
                    total += silence as usize * std::mem::size_of::<i16>();
                }
            }
        }
        Ok(total)
    }

    fn decode_opus(
        &mut self,
        mut data: &[u8],
        output_buffer: &mut [u8],
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let mut total = 0;
        while data.len() > 2 {
            let (len, remainder) = read_u16(data)?;
            data = remainder;
            if len == u16::MAX {
                self.decoder.reset_state()?;
                self.seq = 0;
                continue;
            }
            let (seq, remainder) = read_u16(data)?;
            data = remainder;

            if seq < self.seq {
                self.decoder.reset_state()?;
            } else {
                let lost = (seq - self.seq).min(10);
                for _ in 0..lost {
                    let count = (self.decode_fn)(&mut self.decoder, &[], &mut output_buffer[total..])?;
                    total += count;
                    if total >= output_buffer.len() {
                        return Err("InsufficientOutputBuffer".into());
                    }
                }
            }
            let len = len as usize;

            self.seq = seq + 1;

            if data.len() < len {
                return Err("InsufficientData".into());
            }

            let count = (self.decode_fn)(&mut self.decoder, &data[0..len], &mut output_buffer[total..])?;
            data = &data[len..];
            total += count;
            if total >= output_buffer.len() {
                return Err("InsufficientOutputBuffer".into());
            }
        }

        Ok(total)
    }
}
