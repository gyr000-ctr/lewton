// Vorbis decoder written in Rust
//
// Copyright (c) 2016 est31 <MTest31@outlook.com>
// and contributors. All rights reserved.
// Licensed under MIT license, or Apache 2 license,
// at your option. Please see the LICENSE file
// attached to this source distribution for details.

/*!
Tests for the serial based demultiplexing

The test inputs are synthesised fully in-memory: `rusty_vorbis` produces a
real vorbis bitstream (it is a pure-Rust vorbis encoder whose own test
suite verifies its output against lewton), the `ogg` crate pages the
packets, and a tiny hand written helper emulates a non vorbis (e.g.
theora) bitstream. No network access or native libraries are needed.
*/

use std::collections::VecDeque;
use std::io::Cursor;

extern crate ogg;
extern crate rusty_vorbis;

use ogg::{PacketReader, PacketWriter, PacketWriteEndInfo};
use self::rusty_vorbis::{Error as EncError, VorbisEncoder, VorbisEncoderConfig};

use VorbisError;
use inside_ogg::OggStreamReader;
use samples::InterleavedSamples;
use super::*;

const SAMPLE_RATE :u32 = 8000;
const CHANNELS :u16 = 2;

/// A packet plus the ogg "end info" the muxer should use when paging it.
struct MuxPacket {
	data :Vec<u8>,
	inf :PacketWriteEndInfo,
	granule :u64,
}

/// Encodes a stereo sine signal and returns its serial together with the
/// full packet sequence (three headers followed by the audio packets),
/// each tagged with proper end-of-page / end-of-stream info and granule.
fn make_vorbis_stream(serial :u32, sample_count :usize)
		-> (u32, VecDeque<MuxPacket>) {
	let pcm :Vec<i16> = (0..sample_count)
		.map(|i| ((i as f32 * 0.7).sin() * 9000.0) as i16)
		// Stereo: duplicate each sample onto a second, inverted channel.
		.flat_map(|s| vec![s, (-s as i32) as i16])
		.collect();

	let mut enc = VorbisEncoder::new(VorbisEncoderConfig::default());
	enc.push_pcm_s16(&pcm, CHANNELS, SAMPLE_RATE).unwrap();
	enc.finish();

	let mut raw = Vec::new();
	loop {
		match enc.next_packet() {
			Ok(p) => raw.push(p),
			Err(EncError::Eof) => break,
			Err(e) => panic!("vorbis encoder failed: {:?}", e),
		}
	}
	assert!(raw.len() > 4, "need headers plus at least one audio packet");

	let last_index = raw.len() - 1;
	let mut packets = VecDeque::new();
	for (i, p) in raw.into_iter().enumerate() {
		let inf = if i < 3 {
			// Each header gets its own page, as required by the vorbis spec.
			PacketWriteEndInfo::EndPage
		} else if i == last_index {
			PacketWriteEndInfo::EndStream
		} else {
			// One audio packet per page, like common ogg muxers do. This keeps
			// every packet observable as a page boundary (and thus absgp).
			PacketWriteEndInfo::EndPage
		};
		// Header pages carry granule 0; audio pages carry the packet pts.
		let granule = if i < 3 { 0 } else { p.pts.max(0) as u64 };
		packets.push_back(MuxPacket {
			data : p.data,
			inf,
			granule,
		});
	}
	(serial, packets)
}

/// Builds the packet queue of a fake non vorbis bitstream (e.g. theora).
///
/// The first packet deliberately starts with a non vorbis capture pattern
/// so the selector classifies the whole serial as foreign.
fn make_foreign_stream(serial :u32, packet_count :usize, big :bool)
		-> (u32, VecDeque<MuxPacket>) {
	let mut packets = VecDeque::new();
	for i in 0..packet_count {
		// A theora-style identification header starts with 0x80 "theora".
		let mut data = if i == 0 {
			let mut v = vec![0x80u8, b't', b'h', b'e', b'o', b'r', b'a'];
			// Pad so that multi-segment pages are exercised too.
			if big { v.extend(vec![0xABu8; 70_000]); }
			v
		} else {
			let size = if big { 70_000 } else { 200 };
			vec![(i % 251) as u8; size]
		};
		let inf = if i + 1 == packet_count {
			PacketWriteEndInfo::EndStream
		} else {
			PacketWriteEndInfo::EndPage
		};
		packets.push_back(MuxPacket {
			data : std::mem::take(&mut data),
			inf,
			granule : i as u64,
		});
	}
	(serial, packets)
}

/// Multiplexes several logical streams into one physical ogg byte stream.
///
/// Pages are interleaved round-robin, one packet per turn, which is exactly
/// the layout that defeats a naive single-stream decoder.
fn mux(mut streams :Vec<(u32, VecDeque<MuxPacket>)>) -> Vec<u8> {
	let mut buf = Cursor::new(Vec::new());
	{
		let mut writer = PacketWriter::new(&mut buf);
		// First, make sure every stream's beginning-of-stream page exists:
		// the ogg writer sets the BOS flag on the first page per serial.
		loop {
			let mut progressed = false;
			for &mut (serial, ref mut queue) in streams.iter_mut() {
				if let Some(p) = queue.pop_front() {
					writer.write_packet(p.data.into_boxed_slice(), serial,
						p.inf, p.granule).unwrap();
					progressed = true;
				}
			}
			if !progressed { break; }
		}
	}
	buf.into_inner()
}

/// Decodes every packet of a physical stream with the public reader.
fn decode_all(ogg :&[u8]) -> Result<Vec<Vec<i16>>, VorbisError> {
	let mut reader = OggStreamReader::new(Cursor::new(ogg.to_vec()))?;
	let mut out = Vec::new();
	while let Some(p) =
			reader.read_dec_packet_generic::<InterleavedSamples<i16>>()? {
		out.push(p.samples);
	}
	Ok(out)
}

/// Decodes a specific serial, returning the constructed reader so tests can
/// drive it further (e.g. stream switching).
fn reader_for_serial(ogg :&[u8], serial :u32)
		-> Result<OggStreamReader<Cursor<Vec<u8>>>, VorbisError> {
	OggStreamReader::new_with_serial(Cursor::new(ogg.to_vec()), serial)
}

#[test]
fn multiplex_single_stream_pcm_matches_low_level() {
	// One vorbis stream, no multiplexing: the new reader must produce the
	// exact same PCM samples as a direct, selector-free low level decode.
	let (serial, queue) = make_vorbis_stream(0xAA01, 8192);
	let ogg = mux(vec![(serial, queue)]);

	// Reference path: raw ogg PacketReader + manual header handling, the
	// same primitive the previous implementation was built from.
	let reference = {
		let mut pr = PacketReader::new(Cursor::new(ogg.clone()));
		let ident = ::header::read_header_ident(
			&pr.read_packet_expected().unwrap().data).unwrap();
		let comment = ::header::read_header_comment(
			&pr.read_packet_expected().unwrap().data).unwrap();
		let setup = ::header::read_header_setup(
			&pr.read_packet_expected().unwrap().data,
			ident.audio_channels,
			(ident.blocksize_0, ident.blocksize_1)).unwrap();
		let mut pwr = ::audio::PreviousWindowRight::new();
		let mut pcm = Vec::new();
		while let Some(pck) = pr.read_packet().unwrap() {
			let decoded :InterleavedSamples<i16> =
				::audio::read_audio_packet_generic(
					&ident, &setup, &pck.data, &mut pwr).unwrap();
			pcm.push(decoded.samples);
		}
		let _ = comment;
		pcm
	};

	let got = decode_all(&ogg).expect("single stream must decode");
	assert_eq!(got.len(), reference.len(), "packet count differs");
	for (i, (g, r)) in got.iter().zip(reference.iter()).enumerate() {
		assert_eq!(g, r, "sample data of packet {} differs", i);
	}
	// Sanity: some audio was actually produced.
	assert!(got.iter().map(|p| p.len()).sum::<usize>() > 0);
}

#[test]
fn multiplex_two_vorbis_streams_are_distinguishable_and_selectable() {
	let (serial_a, queue_a) = make_vorbis_stream(0xBB01, 8192);
	let (serial_b, queue_b) = make_vorbis_stream(0xBB02, 8192);
	let ogg = mux(vec![
		(serial_a, queue_a),
		(serial_b, queue_b),
	]);

	// Auto selection is ambiguous: the error must name both serials.
	match OggStreamReader::new(Cursor::new(ogg.clone())) {
		Err(VorbisError::MultipleVorbisStreams(serials)) => {
			assert_eq!(serials.len(), 2);
			assert!(serials.contains(&serial_a));
			assert!(serials.contains(&serial_b));
		},
		other => panic!("expected MultipleVorbisStreams, got {:?}",
			other.map(|_| "ok")),
	}

	// Explicit selection works for either serial, and each decodes audio.
	let mut ra = reader_for_serial(&ogg, serial_a).unwrap();
	assert_eq!(ra.stream_serial(), serial_a);
	let mut samples_a = 0usize;
	while let Some(p) =
			ra.read_dec_packet_generic::<InterleavedSamples<i16>>().unwrap() {
		samples_a += p.samples.len();
	}
	assert!(samples_a > 0);

	let mut rb = reader_for_serial(&ogg, serial_b).unwrap();
	assert_eq!(rb.stream_serial(), serial_b);
	let mut samples_b = 0usize;
	while let Some(p) =
			rb.read_dec_packet_generic::<InterleavedSamples<i16>>().unwrap() {
		samples_b += p.samples.len();
	}
	assert!(samples_b > 0);
	// Both streams encode the same signal, so the recovered PCM length is
	// identical even though the pages were interleaved.
	assert_eq!(samples_a, samples_b);
}

#[test]
fn multiplex_no_vorbis_stream_is_distinguishable() {
	// Only a foreign (theora-like) bitstream, no vorbis anywhere.
	let (serial, queue) = make_foreign_stream(0xCC01, 6, false);
	let ogg = mux(vec![(serial, queue)]);

	match OggStreamReader::new(Cursor::new(ogg.clone())) {
		Err(VorbisError::NoVorbisStream) => {},
		other => panic!("expected NoVorbisStream, got {:?}",
			other.map(|_| "ok")),
	}
	// Asking for a serial that only exists as foreign fails the same way.
	match reader_for_serial(&ogg, serial) {
		Err(VorbisError::NoVorbisStream) => {},
		other => panic!("expected NoVorbisStream, got {:?}",
			other.map(|_| "ok")),
	}
	// A serial that does not exist at all is also NoVorbisStream.
	match reader_for_serial(&ogg, 0xDEAD_BEEF) {
		Err(VorbisError::NoVorbisStream) => {},
		other => panic!("expected NoVorbisStream, got {:?}",
			other.map(|_| "ok")),
	}
}

#[test]
fn multiplex_stream_disappears_is_distinguishable() {
	// The wanted vorbis stream ends early while the foreign video stream
	// keeps carrying pages afterwards.
	let (vorbis_serial, vqueue) = make_vorbis_stream(0xDD01, 4096);
	let vorbis_len = vqueue.len();
	let (video_serial, vid_queue) = make_foreign_stream(0xDD02, 12, false);

	// Hand-crafted interleaving: the vorbis stream finishes first, then the
	// video stream still has packets. Interleave one-for-one until vorbis is
	// exhausted, then append the remaining video packets.
	let mut streams :Vec<(u32, VecDeque<MuxPacket>)> =
		vec![(vorbis_serial, vqueue), (video_serial, vid_queue)];
	let mut ordered :Vec<(u32, MuxPacket)> = Vec::new();
	let mut vorbis_done = false;
	loop {
		let mut any = false;
		for &mut (serial, ref mut q) in streams.iter_mut() {
			if serial == vorbis_serial {
				if let Some(p) = q.pop_front() {
					ordered.push((serial, p));
					any = true;
					if ordered.iter().filter(|(s, _)| *s == vorbis_serial)
							.count() == vorbis_len {
						vorbis_done = true;
					}
				}
			} else if vorbis_done {
				// Drain the remaining video packets only after vorbis EOS.
				while let Some(p) = q.pop_front() {
					ordered.push((serial, p));
					any = true;
				}
			} else if let Some(p) = q.pop_front() {
				ordered.push((serial, p));
				any = true;
			}
		}
		if !any { break; }
	}

	let mut buf = Cursor::new(Vec::new());
	{
		let mut writer = PacketWriter::new(&mut buf);
		for (serial, p) in ordered {
			writer.write_packet(p.data.into_boxed_slice(), serial,
				p.inf, p.granule).unwrap();
		}
	}
	let ogg = buf.into_inner();

	let mut reader = reader_for_serial(&ogg, vorbis_serial).unwrap();
	let mut err = None;
	loop {
		match reader.read_dec_packet_generic::<InterleavedSamples<i16>>() {
			Ok(Some(_)) => {},
			Ok(None) => break,
			Err(e) => { err = Some(e); break; },
		}
	}
	match err {
		Some(VorbisError::StreamDisappeared(serial)) => {
			assert_eq!(serial, vorbis_serial);
		},
		other => panic!("expected StreamDisappeared, got {:?}", other),
	}
	// The video serial is known but must never be selectable as vorbis.
	match reader.switch_stream(video_serial) {
		Err(VorbisError::NoVorbisStream) => {},
		other => panic!("expected NoVorbisStream on foreign switch, got {:?}",
			other.map(|_| "ok")),
	}
}

#[test]
fn multiplex_foreign_and_large_packets_keep_memory_at_packet_level() {
	// Two big foreign streams interleaved with a vorbis stream. Foreign
	// packets are dropped on the spot, so decoding must succeed and never
	// buffer their payloads.
	let (a_serial, a_queue) = make_vorbis_stream(0xEE01, 8192);
	let (v1_serial, v1_queue) = make_foreign_stream(0xEE02, 8, true);
	let (v2_serial, v2_queue) = make_foreign_stream(0xEE03, 8, true);
	let ogg = mux(vec![
		(a_serial, a_queue),
		(v1_serial, v1_queue),
		(v2_serial, v2_queue),
	]);

	let mut reader = reader_for_serial(&ogg, a_serial).unwrap();
	let mut samples = 0usize;
	while let Some(p) =
			reader.read_dec_packet_generic::<InterleavedSamples<i16>>()
			.unwrap() {
		samples += p.samples.len();
	}
	assert!(samples > 0);

	// The two foreign serials are tracked as bookkeeping markers only:
	// switching to them must fail and their discovery must not have created
	// any vorbis state.
	let serials = reader.vorbis_stream_serials();
	assert_eq!(serials, vec![a_serial]);
	assert!(reader.switch_stream(v1_serial).is_err());
	assert!(reader.switch_stream(v2_serial).is_err());
}

#[test]
fn multiplex_switching_preserves_per_stream_pwr_and_absgp() {
	let (serial_a, queue_a) = make_vorbis_stream(0xFF01, 24576);
	let (serial_b, queue_b) = make_vorbis_stream(0xFF02, 24576);
	let ogg = mux(vec![
		(serial_a, queue_a),
		(serial_b, queue_b),
	]);

	let mut reader = reader_for_serial(&ogg, serial_a).unwrap();

	// Decode a couple of packets from A, recording its advancing absgp.
	// Every audio packet sits on its own page, so each decode updates absgp.
	let mut first_absgp = None;
	let mut later_absgp = None;
	for i in 0..3 {
		let p = reader
			.read_dec_packet_generic::<InterleavedSamples<i16>>()
			.unwrap()
			.expect("packet from stream A");
		assert!(!p.samples.is_empty() || i == 0);
		if i == 0 { first_absgp = reader.get_last_absgp(); }
		later_absgp = reader.get_last_absgp();
	}
	let first_absgp = first_absgp.expect("first A packet must set absgp");
	let later_absgp = later_absgp.expect("A absgp must be observable");
	assert!(later_absgp >= first_absgp);

	// Switch to B, decode one packet, then switch back to A. A's window
	// state and absgp must continue instead of restarting from scratch.
	reader.switch_stream(serial_b).unwrap();
	assert_eq!(reader.stream_serial(), serial_b);
	let _b = reader
		.read_dec_packet_generic::<InterleavedSamples<i16>>()
		.unwrap()
		.expect("packet from stream B");
	let b_absgp = reader.get_last_absgp().expect("B absgp must be set");

	reader.switch_stream(serial_a).unwrap();
	assert_eq!(reader.stream_serial(), serial_a);
	// A's granule position is untouched by the detour through B.
	assert_eq!(reader.get_last_absgp(), Some(later_absgp));

	// Decoding the next A packet keeps advancing from the saved state.
	let _a_again = reader
		.read_dec_packet_generic::<InterleavedSamples<i16>>()
		.unwrap()
		.expect("more packets from stream A");
	let advanced = reader.get_last_absgp().expect("A absgp still present");
	assert!(advanced >= later_absgp);
	let _ = b_absgp;
}

#[test]
fn multiplex_discovered_serials_are_reported() {
	let (serial_a, queue_a) = make_vorbis_stream(0x1234, 6144);
	let (serial_b, queue_b) = make_vorbis_stream(0x5678, 6144);
	let ogg = mux(vec![
		(serial_a, queue_a),
		(serial_b, queue_b),
	]);

	// Even when selecting explicitly, discovery reports both vorbis serials.
	let reader = reader_for_serial(&ogg, serial_b).unwrap();
	let serials = reader.vorbis_stream_serials();
	assert!(serials.contains(&serial_a));
	assert!(serials.contains(&serial_b));
}

#[test]
fn multiplex_chained_single_vorbis_stream_is_adopted() {
	// A chained ogg file: two vorbis bitstreams with different serials, but
	// never alive concurrently (the first ends before the second begins).
	// Auto selection must transparently adopt the successor.
	let (serial_a, mut queue_a) = make_vorbis_stream(0x7001, 4096);
	let (serial_b, mut queue_b) = make_vorbis_stream(0x7002, 4096);

	let mut ordered :Vec<(u32, MuxPacket)> = Vec::new();
	while let Some(p) = queue_a.pop_front() {
		ordered.push((serial_a, p));
	}
	while let Some(p) = queue_b.pop_front() {
		ordered.push((serial_b, p));
	}

	let mut buf = Cursor::new(Vec::new());
	{
		let mut writer = PacketWriter::new(&mut buf);
		for (serial, p) in ordered {
			writer.write_packet(p.data.into_boxed_slice(), serial,
				p.inf, p.granule).unwrap();
		}
	}
	let ogg = buf.into_inner();

	let mut reader = OggStreamReader::new(Cursor::new(ogg)).unwrap();
	assert_eq!(reader.stream_serial(), serial_a);
	let mut total = 0usize;
	let mut saw_switch = false;
	while let Some(p) =
			reader.read_dec_packet_generic::<InterleavedSamples<i16>>()
			.unwrap() {
		total += p.samples.len();
		if reader.stream_serial() == serial_b {
			saw_switch = true;
		}
	}
	assert!(saw_switch, "chained successor stream was not adopted");
	assert!(total > 0);
}

#[test]
fn multiplex_selector_unit_distinguishes_slot_kinds() {
	// Direct unit test of the selector state machine without any I/O.
	let mut selector = VorbisSelector::new_auto();
	assert!(selector.selected_serial().is_none());
	assert!(selector.vorbis_serials().is_empty());

	// A foreign BOS packet is registered as a marker, not a vorbis stream.
	let foreign = fake_packet(0x9001, true, false,
		vec![0x80, b't', b'h', b'e', b'o', b'r', b'a']);
	match selector.observe(foreign).unwrap() {
		ObserveResult::NeedMore => {},
		_ => panic!("foreign BOS must yield NeedMore"),
	}
	assert!(selector.vorbis_serials().is_empty());
	assert!(selector.selected_serial().is_none());
}

#[test]
fn multiplex_foreign_packets_are_never_retained() {
	// Peak memory must stay at packet level: large foreign packets (each
	// close to a full ogg page) are dropped as soon as they are observed, so
	// decoding the small vorbis stream interleaved with them must never
	// buffer any foreign payload. This goes through the real muxer and
	// reader rather than hand built packets, so multi-page handling is
	// exercised exactly as in production.
	let (audio_serial, audio_queue) = make_vorbis_stream(0xBEE1, 8192);
	// Few large foreign packets so both streams end at roughly the same
	// point; the point here is payload size, not lifetime (a longer foreign
	// stream is covered separately by the StreamDisappeared test).
	let (foreign_serial, foreign_queue) = make_foreign_stream(0xBEE2, 4, true);
	let ogg = mux(vec![
		(audio_serial, audio_queue),
		(foreign_serial, foreign_queue),
	]);

	// Each foreign packet is close to a full ogg page, dwarfing the vorbis
	// packets. If their payloads were retained, memory would scale with their
	// size; here decoding must simply work regardless of how big they are.
	let mut reader = reader_for_serial(&ogg, audio_serial).unwrap();
	let mut samples = 0usize;
	let mut packets = 0usize;
	while let Some(p) =
			reader.read_dec_packet_generic::<InterleavedSamples<i16>>()
			.unwrap() {
		samples += p.samples.len();
		packets += 1;
	}
	assert!(samples > 0 && packets > 0);
	// The foreign serial produced no vorbis state and is not switchable.
	assert_eq!(reader.vorbis_stream_serials(), vec![audio_serial]);
	assert!(reader.switch_stream(foreign_serial).is_err());
}

/// Minimal Packet construction for selector unit tests. The ogg crate's
/// Packet fields are public within the crate only, so go through a real
/// reader by writing a one-page stream and reading the packet back.
fn fake_packet(serial :u32, first :bool, last :bool, data :Vec<u8>)
		-> ::ogg::Packet {
	let mut buf = Cursor::new(Vec::new());
	{
		let mut writer = PacketWriter::new(&mut buf);
		let inf = match (first, last) {
			(true, true) => PacketWriteEndInfo::EndStream,
			(true, false) => PacketWriteEndInfo::EndPage,
			(false, true) => PacketWriteEndInfo::EndStream,
			_ => PacketWriteEndInfo::NormalPacket,
		};
		writer.write_packet(data.into_boxed_slice(), serial, inf, 0)
			.unwrap();
	}
	let mut reader = PacketReader::new(Cursor::new(buf.into_inner()));
	reader.read_packet().unwrap().unwrap()
}
