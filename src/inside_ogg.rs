// Vorbis decoder written in Rust
//
// Copyright (c) 2016 est31 <MTest31@outlook.com>
// and contributors. All rights reserved.
// Licensed under MIT license, or Apache 2 license,
// at your option. Please see the LICENSE file
// attached to this source distribution for details.

/*!
Higher-level utilities for Ogg streams and files

This module provides higher level access to the library functionality,
and useful helper methods for the Ogg `PacketReader` struct.

It supports physical Ogg streams that contain more than one logical
bitstream, like `.ogv` files with an audio and a video track: the
vorbis bitstream to decode is selected by its stream serial number,
pages and packets of the other bitstreams are skipped during a single
forward scan. See the `multiplex` module for the details.
*/

use ogg::{PacketReader, Packet};
use std::io::{Read, Seek};
use header::*;
use VorbisError;
use audio::{PreviousWindowRight,
	get_decoded_sample_count, read_audio_packet_generic};
use header::HeaderSet;
use multiplex::{MultiplexReader, VorbisSelector};
use samples::{Samples, InterleavedSamples};

/// Reads the three vorbis headers from an ogg stream as well as stream serial information
///
/// Please note that this function doesn't work well with async
/// I/O. In order to support this use case, enable the `async_ogg` feature,
/// and use the `HeadersReader` struct instead.
///
/// If the physical stream contains several logical bitstreams, the first
/// vorbis bitstream is chosen and the packets of the other bitstreams are
/// skipped. As with the single stream case, all unread packets (here: the
/// audio packets already parsed out of the scanned pages) are deleted on
/// return, so the reader has to be fed fresh pages afterwards.
pub fn read_headers<T: Read + Seek>(rdr: &mut PacketReader<T>) ->
		Result<(HeaderSet, u32), VorbisError> {
	let pck :Packet = try!(rdr.read_packet_expected());
	let ident_hdr = try!(read_header_ident(&pck.data));
	let stream_serial = pck.stream_serial();

	let mut pck :Packet = try!(rdr.read_packet_expected());
	while pck.stream_serial() != stream_serial {
		pck = try!(rdr.read_packet_expected());
	}
	let comment_hdr = try!(read_header_comment(&pck.data));

	let mut pck :Packet = try!(rdr.read_packet_expected());
	while pck.stream_serial() != stream_serial {
		pck = try!(rdr.read_packet_expected());
	}
	let setup_hdr = try!(read_header_setup(&pck.data, ident_hdr.audio_channels,
		(ident_hdr.blocksize_0, ident_hdr.blocksize_1)));

	rdr.delete_unread_packets();
	return Ok(((ident_hdr, comment_hdr, setup_hdr), stream_serial));
}

/**
Reading ogg/vorbis files or streams

This is a small helper struct to help reading ogg/vorbis files
or streams in that format.

It supports physical Ogg streams with multiple logical bitstreams, like
`.ogv` files where vorbis is only one of several tracks. In that case the
vorbis bitstream to decode is selected by its stream serial number, via
[`OggStreamReader::new_with_serial`]. The pages and packets of the other
bitstreams are skipped during a single, forward only scan, and their data
is never buffered, so the peak memory usage stays at packet level.

When using [`OggStreamReader::new`] without an explicit serial, the first
vorbis bitstream is selected. If several concurrent vorbis bitstreams are
encountered this way, a [`VorbisError::MultipleVorbisStreams`] error with
their serial numbers is returned and the construction can be retried with
an explicit choice.
*/
pub struct OggStreamReader<T: Read + Seek> {
	multiplex :MultiplexReader<T>,

	stream_serial :u32,

	pub ident_hdr :IdentHeader,
	pub comment_hdr :CommentHeader,
	pub setup_hdr :SetupHeader,
}

impl<T: Read + Seek> OggStreamReader<T> {
	/// Constructs a new OggStreamReader from a given implementation of `Read + Seek`.
	///
	/// The first vorbis bitstream of the physical stream is selected. If the
	/// container holds more than one concurrent vorbis bitstream, a
	/// [`VorbisError::MultipleVorbisStreams`] error is returned containing
	/// the serial numbers of the discovered vorbis bitstreams; use
	/// [`OggStreamReader::new_with_serial`] in that case.
	///
	/// Please note that this function doesn't work well with async
	/// I/O. In order to support this use case, enable the `async_ogg` feature,
	/// and use the `HeadersReader` struct instead.
	pub fn new(rdr :T) ->
			Result<Self, VorbisError> {
		OggStreamReader::from_ogg_reader(PacketReader::new(rdr))
	}

	/// Constructs a new OggStreamReader that decodes the logical vorbis
	/// bitstream with the given stream serial.
	///
	/// This is the entry point for multiplexed containers such as `.ogv`,
	/// where one physical Ogg stream carries e.g. a theora video and a
	/// vorbis audio track. All pages are scanned once in forward order, and
	/// every packet belonging to another bitstream is dropped immediately.
	///
	/// If no bitstream with that serial exists, or it isn't vorbis, a
	/// [`VorbisError::NoVorbisStream`] error is returned. If the selected
	/// bitstream ends while other bitstreams still carry data, decoding
	/// fails later with a [`VorbisError::StreamDisappeared`] error instead
	/// of silently continuing with another stream.
	pub fn new_with_serial(rdr :T, stream_serial :u32) ->
			Result<Self, VorbisError> {
		OggStreamReader::from_ogg_reader_with_serial(
			PacketReader::new(rdr), stream_serial)
	}

	/// Constructs a new OggStreamReader from a given Ogg PacketReader.
	///
	/// The `new` function is a nice wrapper around this function that
	/// also creates the ogg reader.
	///
	/// Please note that this function doesn't work well with async
	/// I/O. In order to support this use case, enable the `async_ogg` feature,
	/// and use the `HeadersReader` struct instead.
	pub fn from_ogg_reader(rdr :PacketReader<T>) ->
			Result<Self, VorbisError> {
		Self::construct(rdr, VorbisSelector::new_auto())
	}

	/// Constructs a new OggStreamReader from a given Ogg PacketReader,
	/// selecting the vorbis bitstream with the given stream serial.
	pub fn from_ogg_reader_with_serial(rdr :PacketReader<T>,
			stream_serial :u32) -> Result<Self, VorbisError> {
		Self::construct(rdr, VorbisSelector::new_with_serial(stream_serial))
	}

	fn construct(rdr :PacketReader<T>, selector :VorbisSelector)
			-> Result<Self, VorbisError> {
		let mut multiplex = MultiplexReader::new(rdr, selector);
		try!(multiplex.initialise());
		let stream_serial = multiplex.selector().selected_serial()
			.expect("initialise guarantees a selected stream");
		let (ident_hdr, comment_hdr, setup_hdr) = {
			let state = multiplex.selector().selected_state();
			(state.ident_hdr.clone(), state.comment_hdr.clone(),
				state.setup_hdr.clone())
		};
		Ok(OggStreamReader {
			multiplex,
			stream_serial,
			ident_hdr,
			comment_hdr,
			setup_hdr,
		})
	}

	pub fn into_inner(self) -> PacketReader<T> {
		self.multiplex.into_inner()
	}

	/// Switches decoding to another, already discovered vorbis bitstream.
	///
	/// The decoding state (window overlap and absolute granule position) is
	/// kept per stream serial, so after switching back a stream continues
	/// exactly where it left off. Switching is only possible to a vorbis
	/// bitstream whose headers have already been scanned; a
	/// [`VorbisError::NoVorbisStream`] error is returned otherwise.
	///
	/// Because the underlying scan is forward only, packets of the
	/// previously selected stream that haven't been read yet are skipped
	/// and cannot be recovered after the switch.
	pub fn switch_stream(&mut self, stream_serial :u32)
			-> Result<(), VorbisError> {
		try!(self.multiplex.selector().switch_to(stream_serial));
		self.stream_serial = stream_serial;
		let (ident_hdr, comment_hdr, setup_hdr) = {
			let state = self.multiplex.selector().selected_state();
			(state.ident_hdr.clone(), state.comment_hdr.clone(),
				state.setup_hdr.clone())
		};
		self.ident_hdr = ident_hdr;
		self.comment_hdr = comment_hdr;
		self.setup_hdr = setup_hdr;
		Ok(())
	}

	/// Returns the serial numbers of all vorbis bitstreams discovered so
	/// far, in the order their first pages appeared.
	pub fn vorbis_stream_serials(&self) -> Vec<u32> {
		self.multiplex.selector_ref().vorbis_serials().to_vec()
	}

	/// Copies the header fields of the selected stream into the public
	/// header fields, e.g. after a chained ogg stream was adopted.
	fn sync_public_headers(&mut self) {
		let (ident_hdr, comment_hdr, setup_hdr) = {
			let state = self.multiplex.selector().selected_state();
			(state.ident_hdr.clone(), state.comment_hdr.clone(),
				state.setup_hdr.clone())
		};
		self.ident_hdr = ident_hdr;
		self.comment_hdr = comment_hdr;
		self.setup_hdr = setup_hdr;
	}

	fn read_next_audio_packet(&mut self) -> Result<Option<Packet>, VorbisError> {
		let pck = match try!(self.multiplex.next_audio_packet()) {
			Some(pck) => pck,
			None => return Ok(None),
		};
		// A chained ogg file may have transparently switched the selected
		// stream to a new serial.
		if let Some(serial) = self.multiplex.selector().selected_serial() {
			if serial != self.stream_serial {
				self.stream_serial = serial;
				self.sync_public_headers();
			}
		}
		Ok(Some(pck))
	}
	/// Reads and decompresses an audio packet from the stream.
	///
	/// On read errors, it returns Err(e) with the error.
	///
	/// On success, it either returns None, when the end of the
	/// stream has been reached, or Some(packet_data),
	/// with the data of the decompressed packet.
	pub fn read_dec_packet(&mut self) ->
			Result<Option<Vec<Vec<i16>>>, VorbisError> {
		let pck = try!(self.read_dec_packet_generic());
		Ok(pck)
	}

	/// Reads and decompresses an audio packet from the stream (interleaved).
	///
	/// On read errors, it returns Err(e) with the error.
	///
	/// On success, it either returns None, when the end of the
	/// stream has been reached, or Some(packet_data),
	/// with the data of the decompressed packet.
	///
	/// Unlike `read_dec_packet`, this function returns the
	/// interleaved samples.
	pub fn read_dec_packet_itl(&mut self) ->
			Result<Option<Vec<i16>>, VorbisError> {
		let decoded_pck :InterleavedSamples<_> = match try!(self.read_dec_packet_generic()) {
			Some(p) => p,
			None => return Ok(None),
		};
		return Ok(Some(decoded_pck.samples));
	}

	/// Reads and decompresses an audio packet from the stream (generic).
	///
	/// On read errors, it returns Err(e) with the error.
	///
	/// On success, it either returns None, when the end of the
	/// stream has been reached, or Some(packet_data),
	/// with the data of the decompressed packet.
	pub fn read_dec_packet_generic<S :Samples>(&mut self) ->
			Result<Option<S>, VorbisError> {
		let pck = match try!(self.read_next_audio_packet()) {
			Some(p) => p,
			None => return Ok(None),
		};
		self.dec_packet_generic(pck).map(Option::Some)
	}

	#[inline]
	pub fn dec_packet_generic<S :Samples>(&mut self, pck :Packet) ->
			Result<S, VorbisError> {
		let state = self.multiplex.selector().selected_state();
		let mut decoded_pck :S = try!(read_audio_packet_generic(&state.ident_hdr,
			&state.setup_hdr, &pck.data, &mut state.pwr));

		// If this is the last packet in the logical bitstream,
		// we need to truncate it so that its ending matches
		// the absgp of the current page.
		// This is what the spec mandates and also the behaviour
		// of libvorbis.
		if let (Some(absgp), true) = (state.cur_absgp, pck.last_in_stream()) {
			let target_length = pck.absgp_page().saturating_sub(absgp) as usize;
			decoded_pck.truncate(target_length);
		}
		let state = self.multiplex.selector().selected_state();
		if pck.last_in_page() {
			state.cur_absgp = Some(pck.absgp_page());
		} else if let &mut Some(ref mut absgp) = &mut state.cur_absgp {
			*absgp += decoded_pck.num_samples() as u64;
		}
		return Ok(decoded_pck);
	}
	/// Skips the given number of samples
	///
	/// Skips multiple packets without decoding any but the last two, so that
	/// the leftover number of samples to skip is lower than the length of the
	/// returned packet.
	///
	/// The function runs a linear skip algorithm which means that instead of
	/// logarithmic *seeking*, it inspects each packet for its length, and
	/// subtracts the length from the length to skip. This function does no
	/// packet decoding until it arrives at the destination, which makes it
	/// way cheaper than just decoding all packets.
	///
	/// The absolute granule position is always increased in whole-package
	/// increments.
	pub fn skip_samples_linear<S :Samples>(&mut self, to_skip :usize) -> Result<(Option<S>, usize), VorbisError> {
		let mut to_skip = to_skip;
		let mut last_pck :Option<Packet> = None;
		let mut next_pck;

		loop {
			if let Some(p) = try!(self.read_next_audio_packet()) {
				next_pck = p;
			} else {
				return Ok((None, to_skip));
			}
			let state = self.multiplex.selector().selected_state();
			let mut sample_cnt = try!(get_decoded_sample_count(&state.ident_hdr,
				&state.setup_hdr, &next_pck.data));
			// If this is the last packet in the logical bitstream,
			// we need to truncate it so that its ending matches
			// the absgp of the current page.
			// This is what the spec mandates and also the behaviour
			// of libvorbis.
			if let (Some(absgp), true) = (state.cur_absgp, next_pck.last_in_stream()) {
				last_pck = None;
				let target_length = next_pck.absgp_page().saturating_sub(absgp) as usize;
				sample_cnt = sample_cnt.min(target_length);
			}
			if to_skip < sample_cnt {
				// We reached the end of our search.
				if let Some(last_pck) = last_pck {
					let state = self.multiplex.selector().selected_state();
					state.pwr = PreviousWindowRight::new();
					let _decoded_pck :S = try!(read_audio_packet_generic(
						&state.ident_hdr, &state.setup_hdr,
						&last_pck.data, &mut state.pwr));
				}
				let decoded_pck = try!(self.dec_packet_generic(next_pck));
				return Ok((Some(decoded_pck), to_skip));
			} else {
				to_skip -= sample_cnt;
			}
			let state = self.multiplex.selector().selected_state();
			if let &mut Some(ref mut absgp) = &mut state.cur_absgp {
				*absgp += sample_cnt as u64;
			}
			last_pck = Some(next_pck);
		}
	}

	/// Returns the stream serial of the current stream
	///
	/// The stream serial can change in chained ogg files.
	pub fn stream_serial(&self) -> u32 {
		self.stream_serial
	}

	/// Returns the absolute granule position of the last read page.
	///
	/// In the case of ogg/vorbis, the absolute granule position is given
	/// as number of PCM samples, on a per channel basis.
	pub fn get_last_absgp(&self) -> Option<u64> {
		self.multiplex.selector_ref().selected_absgp()
	}

	/// Seeks to the specified absolute granule position, with a page granularity.
	///
	/// The granularity is per-page, and the obtained position is
	/// then <= the seeked absgp.
	///
	/// In the case of ogg/vorbis, the absolute granule position is given
	/// as number of PCM samples, on a per channel basis.
	pub fn seek_absgp_pg(&mut self, absgp :u64) -> Result<(), VorbisError> {
		try!(self.multiplex.reader().seek_absgp(Some(self.stream_serial), absgp));
		// Reset the internal state after the seek
		let state = self.multiplex.selector().selected_state();
		state.cur_absgp = None;
		state.pwr = PreviousWindowRight::new();
		Ok(())
	}
}

#[cfg(feature = "async_ogg")]
/**
Support for async I/O

This module provides support for asyncronous I/O.
*/
pub mod async_api {

	use super::*;
	use audio::read_audio_packet;
	use ogg::OggReadError;
	use ogg::reading::async_api::PacketReader;
	use futures::stream::Stream;
	use tokio_io::AsyncRead;
	use futures::{Async, Future, Poll};
	use std::io::{Error, ErrorKind};
	use std::mem::replace;

	/// Async ready creator utility to read headers out of an
	/// ogg stream.
	///
	/// All functions this struct has are ready to be used for operation with async I/O.
	pub struct HeadersReader<T: AsyncRead> {
		pck_rd :PacketReader<T>,
		ident_hdr :Option<IdentHeader>,
		comment_hdr :Option<CommentHeader>,
	}
	impl<T: AsyncRead> HeadersReader<T> {
		pub fn new(inner :T) -> Self {
			HeadersReader::from_packet_reader(PacketReader::new(inner))
		}
		pub fn from_packet_reader(pck_rd :PacketReader<T>) -> Self {
			HeadersReader {
				pck_rd,
				ident_hdr : None,
				comment_hdr : None,
			}
		}
	}
	impl<T: AsyncRead> Future for HeadersReader<T> {
		type Item = HeaderSet;
		type Error = VorbisError;
		fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
			macro_rules! rd_pck {
				() => {
					if let Some(pck) = try_ready!(self.pck_rd.poll()) {
						pck
					} else {
						// Note: we are stealing the Io variant from
						// the ogg crate here which is not 100% clean,
						// but I think in general it is what the
						// read_packet_expected function of the ogg
						// crate does too, and adding our own case
						// to the VorbisError enum that only fires
						// in an async mode is too complicated IMO.
						try!(Err(OggReadError::ReadError(Error::new(ErrorKind::UnexpectedEof,
							"Expected header packet but found end of stream"))))
					}
				}
			}
			if self.ident_hdr.is_none() {
				let pck = rd_pck!();
				self.ident_hdr = Some(try!(read_header_ident(&pck.data)));
			}
			if self.comment_hdr.is_none() {
				let pck = rd_pck!();
				self.comment_hdr = Some(try!(read_header_comment(&pck.data)));
			}
			let setup_hdr = {
				let ident = self.ident_hdr.as_ref().unwrap();
				let pck = rd_pck!();
				try!(read_header_setup(&pck.data,
					ident.audio_channels, (ident.blocksize_0, ident.blocksize_1)))
			};
			let ident_hdr = replace(&mut self.ident_hdr, None).unwrap();
			let comment_hdr = replace(&mut self.comment_hdr, None).unwrap();
			Ok(Async::Ready((ident_hdr, comment_hdr, setup_hdr)))
		}
	}
	/// Reading ogg/vorbis files or streams
	///
	/// This is a small helper struct to help reading ogg/vorbis files
	/// or streams in that format.
	///
	/// It only supports the main use case of pure audio ogg files streams.
	/// Reading a file where vorbis is only one of multiple streams, like
	/// in the case of ogv, is not supported.
	///
	/// If you need support for this, you need to use the lower level methods
	/// instead.
	pub struct OggStreamReader<T :AsyncRead> {
		pck_rd :PacketReader<T>,
		pwr :PreviousWindowRight,

		pub ident_hdr :IdentHeader,
		pub comment_hdr :CommentHeader,
		pub setup_hdr :SetupHeader,

		absgp_of_last_read :Option<u64>,
	}

	impl<T :AsyncRead> OggStreamReader<T> {
		/// Creates a new OggStreamReader from the given parameters
		pub fn new(hdr_rdr :HeadersReader<T>, hdrs :HeaderSet) -> Self {
			OggStreamReader::from_pck_rdr(hdr_rdr.pck_rd, hdrs)
		}
		/// Creates a new OggStreamReader from the given parameters
		pub fn from_pck_rdr(pck_rd :PacketReader<T>, hdrs :HeaderSet) -> Self {
			OggStreamReader {
				pck_rd,
				pwr : PreviousWindowRight::new(),

				ident_hdr : hdrs.0,
				comment_hdr : hdrs.1,
				setup_hdr : hdrs.2,

				absgp_of_last_read : None,
			}
		}
	}

	impl<T :AsyncRead> Stream for OggStreamReader<T> {
		type Item = Vec<Vec<i16>>;
		type Error = VorbisError;

		fn poll(&mut self) -> Poll<Option<Vec<Vec<i16>>>, VorbisError> {
			let pck = match try_ready!(self.pck_rd.poll()) {
				Some(p) => p,
				None => return Ok(Async::Ready(None)),
			};
			let decoded_pck = try!(read_audio_packet(&self.ident_hdr,
				&self.setup_hdr, &pck.data, &mut self.pwr));
			self.absgp_of_last_read = Some(pck.absgp_page());
			Ok(Async::Ready(Some(decoded_pck)))
		}
	}
}
