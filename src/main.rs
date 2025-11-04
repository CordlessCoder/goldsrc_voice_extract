use clap::Parser;
use dem::{open_demo};
use dem::types::{Demo, EngineMessage, FrameData, MessageData, NetMessage};
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use steam_audio_codec::{SteamVoiceData};
use rsmpeg::{
    avformat::{AVFormatContextOutput},
    avcodec::{AVCodec, AVCodecContext, AVCodecRef},
    ffi::{
        AV_SAMPLE_FMT_S16,
        AV_SAMPLE_FMT_FLT,
        AVRational,
        AV_CODEC_CONFIG_SAMPLE_RATE
    },
    swresample::{SwrContext},
    avutil::{
        AVChannelLayout,
        AVFrame,
        get_bytes_per_sample,
    }
};

mod decoder;
use decoder::SteamVoiceDecoder;

const INITIAL_TIME_PAD_SECONDS: f32 = 0.2;
pub const SAMPLE_RATE: i32 = 24_000;

struct PlayerStream {
    decoded_samples: VecDeque<u8>,
    time_pad: f32,
    playing: bool,
    frame_accum: Vec<u8>,
    codec_ctx: AVCodecContext,
    stream_index: usize,
    frame: AVFrame,
    pts: i64,
    last_demo_pts: i64,
    decoder: SteamVoiceDecoder,
    bytes_per_sample: usize,
    enc_bytes_per_sample: usize,
    resampler: Option<SwrContext>
}

impl PlayerStream {
    fn new(fmt_ctx: &mut AVFormatContextOutput, codec: &AVCodecRef<'static>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut codec_ctx = AVCodecContext::new(codec);

        let channel_layout = AVChannelLayout::from_nb_channels(1).into_inner();
        let supported_fmts = codec_ctx.get_supported_sample_fmts(None)?;

        let (decoder_format, encoder_format) = if supported_fmts.contains(&AV_SAMPLE_FMT_S16) {
            (AV_SAMPLE_FMT_S16, AV_SAMPLE_FMT_S16)
        } else if supported_fmts.contains(&AV_SAMPLE_FMT_FLT) {
            (AV_SAMPLE_FMT_FLT, AV_SAMPLE_FMT_FLT)
        } else {
            let encoder_format = supported_fmts.get(0).copied().ok_or("Encoder does not report supported sample formats?")?;
            (AV_SAMPLE_FMT_S16, encoder_format)
        };

        let supported_rates = unsafe { codec_ctx.get_supported_config::<i32>(None, AV_CODEC_CONFIG_SAMPLE_RATE) }?;
        let encoder_rate = if supported_rates.len() == 0 || supported_rates.contains(&SAMPLE_RATE) {
            SAMPLE_RATE
        } else {
            supported_rates.get(0).copied().expect("Coudln't get first supported rate?")
        };

        let resampler = if decoder_format != encoder_format || SAMPLE_RATE != encoder_rate {
            let mut swr = SwrContext::new(
                &channel_layout,
                encoder_format,
                encoder_rate,
                &channel_layout,
                decoder_format,
                SAMPLE_RATE
            )?;
            swr.init()?;
            Some(swr)
        } else { None };

        codec_ctx.set_sample_fmt(encoder_format);
        codec_ctx.set_ch_layout(channel_layout);
        codec_ctx.set_sample_rate(encoder_rate);
        codec_ctx.set_time_base(AVRational { num: 1, den: codec_ctx.sample_rate });

        codec_ctx.open(None)?;

        let stream_index = {
            let mut stream = fmt_ctx.new_stream();
            stream.set_codecpar(codec_ctx.extract_codecpar());
            stream.set_time_base(codec_ctx.time_base);
            stream.index as usize
        };

        let mut frame = AVFrame::new();
        let frame_size = if codec_ctx.frame_size > 0 { codec_ctx.frame_size } else { 1024 };
        frame.set_nb_samples(frame_size);
        frame.set_format(codec_ctx.sample_fmt);
        frame.set_ch_layout(codec_ctx.ch_layout);
        frame.set_sample_rate(codec_ctx.sample_rate);
        frame.get_buffer(0)?;

        Ok(Self {
            decoded_samples: VecDeque::new(),
            time_pad: INITIAL_TIME_PAD_SECONDS,
            playing: false,
            frame_accum: Vec::with_capacity(frame_size as usize),
            codec_ctx,
            stream_index,
            frame,
            pts: 0,
            last_demo_pts: 0,
            decoder: SteamVoiceDecoder::new(decoder_format)?,
            bytes_per_sample: get_bytes_per_sample(decoder_format).expect("Couldn't get bytes per sample of sample format???"),
            enc_bytes_per_sample: get_bytes_per_sample(encoder_format).expect("Coudln't get bytes per sample on encoder format?"),
            resampler
       })
    }

    fn append_samples(&mut self, samples: &[u8]) {
        if self.buffered_samples() == 0 { self.time_pad = INITIAL_TIME_PAD_SECONDS; }
        self.decoded_samples.extend(samples.iter());
    }

    fn consume_samples(&mut self, sample_count: usize) -> Vec<u8> {
        let bytes = sample_count * self.bytes_per_sample;
        let mut out = Vec::with_capacity(bytes);
        for _ in 0..bytes {
            if let Some(s) = self.decoded_samples.pop_front() {
                out.push(s);
            } else {
                out.push(0);
            }
        }
        out
    }

    fn buffered_samples(&self) -> usize {
        self.decoded_samples.len()
    }
}

fn discover_players(players: &mut HashMap<u64, PlayerStream>, demo: &Demo, fmt_ctx: &mut AVFormatContextOutput, codec: &AVCodecRef<'static>) {
    for entry in &demo.directory.entries {
        if entry.type_ == 0 { continue; } // nEntryType == DEMO_STARTUP
        for frame in &entry.frames {
            let FrameData::NetworkMessage(ref boxed_network_message) = frame.frame_data else { continue; };
            let network_message = &boxed_network_message.1;
            let MessageData::Parsed(ref messages) = network_message.messages else { continue; };
            for message in messages {
                let NetMessage::EngineMessage(engine_message) = message else { continue; };
                let EngineMessage::SvcVoiceData(ref svc_voice_data) = **engine_message else { continue; };
                let Ok(steam_voice_data) = SteamVoiceData::new(&svc_voice_data.data) else {
                    panic!("Failed to parse steam voice data!");
                };

                let key = steam_voice_data.steam_id;

                players
                    .entry(key)
                    .or_insert_with(|| {
                        PlayerStream::new(fmt_ctx, codec).expect("Creating player stream failed!")
                    });
            }
        }
    }
}

#[derive(Parser, Debug)]
#[command(about, version)]
struct Args {
    /// Input demo file
    #[arg(value_name = "input")]
    input: String,

    /// Codec to use for audio encoding. Infered from output format if not included
    #[arg(short = 'c', value_name = "codec")]
    c: Option<String>,

    /// Audio bitrate for encoder (when relevant)
    #[arg(short = 'b', value_name = "bitrate")]
    b: Option<String>,

    /// Output format. Infered from output file name extension if not included
    #[arg(short = 'f', value_name = "fmt")]
    f: Option<String>,

    /// Output audio file
    #[arg(value_name = "output")]
    output: String,
}


fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let maybe_format_name = args.f.map(|f| CString::new(f).unwrap());

    let mut fmt_ctx = AVFormatContextOutput::builder()
        .maybe_format_name(maybe_format_name.as_ref().map(|c| c.as_c_str()))
        .filename(CString::new(args.output).unwrap().as_c_str())
        .build()?;

    let codec = if let Some(codec) = args.c {
        AVCodec::find_encoder_by_name(CString::new(codec).unwrap().as_c_str()).ok_or("Encoder does not exist")?
    } else {
        AVCodec::find_encoder(fmt_ctx.oformat().audio_codec).expect("Couldn't find encoder from default id!")
    };

    let demo = open_demo(args.input)?;
    let mut players: HashMap<u64, PlayerStream> = HashMap::new();
    discover_players(&mut players, &demo, &mut fmt_ctx, &codec);

    fmt_ctx.write_header(&mut None)?;
    let mut last_frame_time: Option<f32> = None;

    for entry in demo.directory.entries {
        if entry.type_ == 0 { continue; } // nEntryType == DEMO_STARTUP

        for demo_frame in entry.frames {
            'voice: {
                let FrameData::NetworkMessage(boxed_network_message) = demo_frame.frame_data else { break 'voice; };
                let network_message = boxed_network_message.1;
                let MessageData::Parsed(messages) = network_message.messages else { break 'voice; };
                for message in messages {
                    let NetMessage::EngineMessage(engine_message) = message else { continue; };
                    let EngineMessage::SvcVoiceData(svc_voice_data) = *engine_message else { continue; };
                    let Ok(steam_voice_data) = SteamVoiceData::new(&svc_voice_data.data) else {
                        eprintln!("Failed to parse svc_voice_data as steam voice data!");
                        continue;
                    };

                    let key = steam_voice_data.steam_id;

                    let player_stream = players.get_mut(&key).expect("Player stream for found id didn't exist!");

                    // goldsrc interally uses a buffer half this size, but it also has a little
                    // less than half the sample rate. this calculation has always worked, so...
                    let mut tmp = vec![0u8; 8192 * player_stream.bytes_per_sample];
                    match player_stream.decoder.decode(steam_voice_data, &mut tmp) {
                        Ok(samples_written) => {
                            player_stream.append_samples(&tmp[..samples_written]);
                        }
                        Err(e) => {
                            eprintln!("Decoder error: {:?}", e);
                        }
                    }
                }
            }

            let frametime = if let Some(prev) = last_frame_time {
                (demo_frame.time - prev).max(0.0)
            } else {
                0.0
            };
            last_frame_time = Some(demo_frame.time);

            // Although we're looping through parsed "frames", they're really just sections of
            // information about a demo at a given time. These sections will group together on game
            // frames, but there can be mutliple parsed demo "frames" in a game frame with
            // different kinds of information about the game state. We use the frame rate
            // to replicate the engine behavior with audio buffering, but we don't actually care
            // about each frame
            if frametime == 0.0 {
                continue;
            }

            let demo_frame_time_as_pts = (demo_frame.time * SAMPLE_RATE as f32).floor() as i64;

            for (_id, player_stream) in players.iter_mut() {
                if player_stream.time_pad > 0.0 && player_stream.buffered_samples() != 0 {
                    player_stream.time_pad -= frametime;
                    if player_stream.time_pad <= 0.0 {
                        player_stream.playing = true;
                    }
                }

                let demo_frame_sample_count = (demo_frame_time_as_pts - player_stream.last_demo_pts) as usize;
                player_stream.last_demo_pts = demo_frame_time_as_pts;

                let frame_samples = if player_stream.playing {
                    let samples = player_stream.consume_samples(demo_frame_sample_count);
                    if player_stream.buffered_samples() == 0 {
                        player_stream.playing = false;
                    }
                    samples
                } else {
                    vec![0u8; demo_frame_sample_count * player_stream.bytes_per_sample]
                };

                let mut samples: Vec<u8> = Vec::with_capacity(player_stream.frame_accum.len() + frame_samples.len());
                samples.extend(&player_stream.frame_accum);
                samples.extend(frame_samples);

                let mut offset = 0;
                let frame_size_bytes = player_stream.frame.nb_samples as usize * player_stream.bytes_per_sample;

                while offset + frame_size_bytes <= samples.len() {
                    let frame_slice = &samples[offset..offset + frame_size_bytes];

                    let mut resampled_buf;
                    let frame_data: &[u8] = if let Some(resampler) = &mut player_stream.resampler {
                        resampled_buf = vec![0u8; frame_slice.len() * 4];
                        let in_bufs = [frame_slice.as_ptr()];
                        let mut out_bufs = [resampled_buf.as_mut_ptr()];

                        let out_samples = unsafe {
                            resampler.convert(
                                out_bufs.as_mut_ptr(),
                                player_stream.frame.nb_samples as i32,
                                in_bufs.as_ptr(),
                                player_stream.frame.nb_samples as i32,
                            )?
                        } as usize;

                        &resampled_buf[..out_samples * player_stream.enc_bytes_per_sample]
                    } else {
                        frame_slice
                    };

                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            frame_data.as_ptr(),
                            player_stream.frame.data[0] as *mut u8,
                            frame_data.len(),
                        );
                    }

                    player_stream.frame.set_pts(player_stream.pts);
                    player_stream.pts += player_stream.frame.nb_samples as i64;
                    player_stream.codec_ctx.send_frame(Some(&player_stream.frame))?;

                    while let Ok(mut pkt) = player_stream.codec_ctx.receive_packet() {
                        pkt.rescale_ts(
                            player_stream.codec_ctx.time_base,
                            fmt_ctx.streams()[player_stream.stream_index].time_base,
                        );
                        pkt.set_stream_index(player_stream.stream_index as i32);
                        fmt_ctx.write_frame(&mut pkt)?;
                    }

                    offset += frame_size_bytes;
                }

                player_stream.frame_accum = Vec::from(&samples[offset..]);
            }
        }
    }

    // Flush
    for (_id, player_stream) in players.iter_mut() {
        player_stream.codec_ctx.send_frame(None)?;

        while let Ok(mut pkt) = player_stream.codec_ctx.receive_packet() {
            pkt.rescale_ts(player_stream.codec_ctx.time_base, fmt_ctx.streams()[player_stream.stream_index].time_base);
            pkt.set_stream_index(player_stream.stream_index as i32);
            fmt_ctx.write_frame(&mut pkt)?;
        }
    }

    fmt_ctx.write_trailer()?;

    Ok(())
}
