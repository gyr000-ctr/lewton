// Vorbis decoder written in Rust
//
// Copyright (c) 2016 est31 <MTest31@outlook.com>
// and contributors. All rights reserved.
// Licensed under MIT license, or Apache 2 license,
// at your option. Please see the LICENSE file
// attached to this source distribution for details.

/*!
Demultiplexing of physical Ogg streams into logical vorbis bitstreams

An Ogg container can interleave several logical bitstreams, identified by
their stream serial number, inside one physical stream. This is what e.g.
Ogg Theora video (`.ogv`) does: one theora stream and one or more vorbis
audio streams share the same pages.

The [`VorbisSelector`] in this module performs the split at page and packet
granularity:

* Every Ogg page is scanned exactly once, in forward order. Nothing is ever
  seeked back to or re-read, so the module works on any `Read + Seek` the
  underlying `ogg` crate accepts.
* Packets of bitstreams other than the selected one are dropped immediately,
  without their data being retained. The only per-stream state kept is the
  codec state of vorbis bitstreams and a bookkeeping marker for non vorbis
  ones, so the peak memory usage stays at packet level (plus the single page
  the `ogg` crate itself keeps internally).
* The vorbis decoding state (`PreviousWindowRight` and the absolute granule
  position) is stored per stream serial, so switching the selected stream
  and switching back continues each stream with its own state instead of
  restarting it.
*/

use ogg::{Packet, PacketReader};
use std::io::{Read, Seek};

use VorbisError;
use audio::PreviousWindowRight;
use header::*;

/// What the selector made of a packet handed to it.
enum ObserveResult {
	/// The packet belongs to the selected, fully initialised vorbis stream
	/// and has to be decoded.
	Audio(Packet),
	/// The selector needs more packets before it can hand one out.
	NeedMore,
}

/// Bookkeeping for a logical bitstream that turned out not to be vorbis.
///
/// No packet data is ever stored here, the marker only exists to be able to
/// tell known non vorbis bitstreams apart from newly appearing ones.
struct ForeignStream {
	/// Becomes true once the bitstream has signalled its last page.
	eos :bool,
}

/// Per-serial decoding state of a vorbis bitstream.
pub(crate) struct VorbisStreamState {
	pub(crate) ident_hdr :IdentHeader,
	pub(crate) comment_hdr :CommentHeader,
	pub(crate) setup_hdr :SetupHeader,
	pub(crate) pwr :PreviousWindowRight,
	/// Absolute granule position of the last decoded page.
	pub(crate) cur_absgp :Option<u64>,
	/// True once the bitstream's last page (header type flag `0x04`) has
	/// been observed.
	pub(crate) eos :bool,
}

/// Which headers of a not yet fully initialised vorbis bitstream have been
/// collected, in the order mandated by the vorbis spec.
enum PendingVorbis {
	/// Identification header seen, comment header is next.
	Ident(IdentHeader),
	/// Identification and comment headers seen, setup is next.
	Comment(IdentHeader, CommentHeader),
}

enum Slot {
	/// A vorbis bitstream whose three headers haven't all been seen yet.
	Pending(PendingVorbis),
	/// A fully initialised vorbis bitstream with its decoding state.
	Ready(VorbisStreamState),
	/// A bitstream that isn't vorbis.
	Foreign(ForeignStream),
}

impl Slot {
	fn is_ready(&self) -> bool {
		matches!(*self, Slot::Ready(_))
	}
}

/**
Selects one logical vorbis bitstream out of a multiplexed physical Ogg stream.

The selector is driven by feeding it every packet of the physical stream in
order via [`VorbisSelector::observe`]. It discovers the logical bitstreams as
their first pages appear, parses the headers of the vorbis ones, and only
ever returns audio packets of the selected stream.
*/
pub(crate) struct VorbisSelector {
	slots :std::collections::HashMap<u32, Slot>,
	/// Serial of the bitstream whose packets get returned, once determined.
	selected :Option<u32>,
	/// If `true`, any newly discovered vorbis bitstream becomes the selected
	/// one. If `false`, only the bitstream with `wanted` is eligible.
	auto_select :bool,
	/// Serial requested by the caller, when `auto_select` is `false`.
	wanted :Option<u32>,
	/// Serials of every vorbis bitstream discovered so far, in discovery
	/// order. Used for the `MultipleVorbisStreams` error.
	vorbis_serials :Vec<u32>,
}

impl VorbisSelector {
	/// Creates a selector that picks the first vorbis bitstream it finds.
	pub(crate) fn new_auto() -> Self {
		VorbisSelector {
			slots : std::collections::HashMap::new(),
			selected : None,
			auto_select : true,
			wanted : None,
			vorbis_serials : Vec::new(),
		}
	}

	/// Creates a selector that only accepts the bitstream with `serial`.
	pub(crate) fn new_with_serial(serial :u32) -> Self {
		VorbisSelector {
			slots : std::collections::HashMap::new(),
			selected : None,
			auto_select : false,
			wanted : Some(serial),
			vorbis_serials : Vec::new(),
		}
	}

	/// Serials of all vorbis bitstreams discovered so far, in discovery order.
	pub(crate) fn vorbis_serials(&self) -> &[u32] {
		&self.vorbis_serials
	}

	/// Serial of the currently selected bitstream, if its headers are done.
	pub(crate) fn selected_serial(&self) -> Option<u32> {
		match self.selected {
			Some(serial) if self.slots.get(&serial).map_or(false, Slot::is_ready)
				=> Some(serial),
			_ => None,
		}
	}

	/// Accesses the state of the selected, initialised vorbis bitstream.
	pub(crate) fn selected_state(&mut self) -> &mut VorbisStreamState {
		let serial = self.selected.expect("selected stream must exist");
		match self.slots.get_mut(&serial) {
			Some(Slot::Ready(state)) => state,
			_ => panic!("selected stream must be initialised"),
		}
	}

	/// Absolute granule position of the last page decoded for the selected
	/// bitstream.
	pub(crate) fn selected_absgp(&self) -> Option<u64> {
		self.selected_serial().and_then(|serial| {
			match self.slots.get(&serial) {
				Some(Slot::Ready(state)) => state.cur_absgp,
				_ => None,
			}
		})
	}

	/// True if the selected bitstream has signalled its end of stream.
	fn selected_eos(&self) -> bool {
		self.selected.map_or(false, |serial| {
			match self.slots.get(&serial) {
				Some(Slot::Ready(state)) => state.eos,
				_ => false,
			}
		})
	}

	/// Switches the selected bitstream to `serial`.
	///
	/// The per-stream decoding state is preserved, so switching back later
	/// continues where the stream left off. Note that this is a forward only
	/// reader: packets of the previously selected stream that were still
	/// unread at the time of the switch are skipped, they cannot be recovered.
	pub(crate) fn switch_to(&mut self, serial :u32) -> Result<(), VorbisError> {
		match self.slots.get(&serial) {
			Some(Slot::Ready(_)) => {
				self.selected = Some(serial);
				Ok(())
			},
			Some(Slot::Pending(_)) => Err(VorbisError::NoVorbisStream),
			Some(Slot::Foreign(_)) => Err(VorbisError::NoVorbisStream),
			None => Err(VorbisError::NoVorbisStream),
		}
	}

	/// Feeds one packet of the physical stream to the selector.
	fn observe(&mut self, pck :Packet) -> Result<ObserveResult, VorbisError> {
		let serial = pck.stream_serial();
		let first_in_stream = pck.first_in_stream();
		let last_in_stream = pck.last_in_stream();

		// A packet of a bitstream we already know about.
		if let Some(slot) = self.slots.get_mut(&serial) {
			match slot {
				Slot::Foreign(foreign) => {
					if last_in_stream {
						foreign.eos = true;
					}
					return self.after_foreign_packet();
				},
				Slot::Pending(_) => {
					let slot = self.slots.remove(&serial).unwrap();
					let next_slot = self.advance_pending(slot, &pck)?;
					let became_ready = next_slot.is_ready();
					self.slots.insert(serial, next_slot);
					if became_ready {
						self.on_stream_ready(serial)?;
					}
					return Ok(ObserveResult::NeedMore);
				},
				Slot::Ready(state) => {
					if last_in_stream {
						state.eos = true;
					}
					if self.selected == Some(serial) {
						return Ok(ObserveResult::Audio(pck));
					}
					// Packet of an initialised but not selected vorbis
					// bitstream: drop it without retaining any data.
					return Ok(ObserveResult::NeedMore);
				},
			}
		}

		// First packet of a previously unseen bitstream. Per the Ogg spec
		// the first page carries the "first page" flag, which the ogg crate
		// exposes as first_in_stream() on its first packet.
		if !first_in_stream {
			// A page starting mid bitstream, e.g. right after a seek. We
			// cannot initialise a vorbis decoder without its headers, so
			// treat it as foreign and keep scanning.
			self.slots.insert(serial, Slot::Foreign(ForeignStream {
				eos : last_in_stream,
			}));
			return self.after_foreign_packet();
		}

		match read_header_ident(&pck.data) {
			Ok(ident_hdr) => {
				// Every vorbis bitstream gets its headers parsed fully, even
				// the ones we won't decode: this keeps their serial known and
				// lets the caller switch to them later. Whether the stream is
				// eligible for selection is decided once it is fully ready.
				if !self.vorbis_serials.contains(&serial) {
					self.vorbis_serials.push(serial);
				}
				self.slots.insert(serial,
					Slot::Pending(PendingVorbis::Ident(ident_hdr)));
				Ok(ObserveResult::NeedMore)
			},
			// Any header parse failure other than "this simply isn't a
			// vorbis stream" is a real error and gets propagated.
			Err(HeaderReadError::NotVorbisHeader) |
				Err(HeaderReadError::HeaderIsAudio) => {
				self.slots.insert(serial, Slot::Foreign(ForeignStream {
					eos : last_in_stream,
				}));
				self.after_foreign_packet()
			},
			Err(e) => Err(VorbisError::BadHeader(e)),
		}
	}

	/// Handles the second and third header packets of a vorbis bitstream.
	fn advance_pending(&mut self, slot :Slot, pck :&Packet)
			-> Result<Slot, VorbisError> {
		Ok(match slot {
			Slot::Pending(PendingVorbis::Ident(ident_hdr)) => {
				let comment_hdr = try!(read_header_comment(&pck.data));
				Slot::Pending(PendingVorbis::Comment(ident_hdr, comment_hdr))
			},
			Slot::Pending(PendingVorbis::Comment(ident_hdr, comment_hdr)) => {
				let setup_hdr = try!(read_header_setup(&pck.data,
					ident_hdr.audio_channels,
					(ident_hdr.blocksize_0, ident_hdr.blocksize_1)));
				Slot::Ready(VorbisStreamState {
					ident_hdr,
					comment_hdr,
					setup_hdr,
					pwr : PreviousWindowRight::new(),
					cur_absgp : None,
					eos : false,
				})
			},
			other => other,
		})
	}

	/// Called once a vorbis bitstream has collected all three headers.
	fn on_stream_ready(&mut self, serial :u32) -> Result<(), VorbisError> {
		// With an explicit serial, only the requested stream is ever
		// selected. Other vorbis streams stay parsed but simply skipped.
		if !self.auto_select {
			if self.wanted == Some(serial) {
				self.selected = Some(serial);
			}
			return Ok(());
		}

		// Auto selection.
		match self.selected {
			None => {
				// The first fully ready vorbis stream is the default.
				self.selected = Some(serial);
				Ok(())
			},
			Some(previous) if previous == serial => Ok(()),
			Some(previous) => {
				// A second vorbis stream became ready while the first one
				// is still alive: genuine multiplexing, the choice is
				// ambiguous. If the first one already signalled its end of
				// stream, this is a chained ogg file and the successor is
				// adopted transparently.
				let previous_eos = match self.slots.get(&previous) {
					Some(Slot::Ready(state)) => state.eos,
					_ => false,
				};
				if previous_eos {
					self.selected = Some(serial);
					Ok(())
				} else {
					Err(VorbisError::MultipleVorbisStreams(
						self.vorbis_serials.clone()))
				}
			},
		}
	}

	/// Common handling after observing a packet of a non selected bitstream.
	fn after_foreign_packet(&mut self) -> Result<ObserveResult, VorbisError> {
		if !self.auto_select && self.selected_eos() {
			// Explicit serial selection and the wanted bitstream has
			// already ended while other bitstreams still carry data.
			let serial = self.selected.unwrap();
			return Err(VorbisError::StreamDisappeared(serial));
		}
		Ok(ObserveResult::NeedMore)
	}
}

/**
Drives a [`VorbisSelector`] from an Ogg [`PacketReader`]

This wraps the low level selector together with its packet source. All
scanning happens in a single forward pass: every page is read once and every
non selected packet is dropped on the spot.
*/
pub(crate) struct MultiplexReader<T :Read + Seek> {
	rdr :PacketReader<T>,
	selector :VorbisSelector,
	/// The first audio packet of the selected stream, pulled ahead during
	/// initialisation so that a concurrently multiplexed second vorbis
	/// stream has enough pages to reveal itself. At most one packet is ever
	/// buffered here, keeping peak memory at packet level.
	primed :Option<Packet>,
}

impl<T :Read + Seek> MultiplexReader<T> {
	pub(crate) fn new(rdr :PacketReader<T>, selector :VorbisSelector) -> Self {
		MultiplexReader { rdr, selector, primed : None }
	}

	pub(crate) fn reader(&mut self) -> &mut PacketReader<T> {
		&mut self.rdr
	}

	pub(crate) fn selector(&mut self) -> &mut VorbisSelector {
		&mut self.selector
	}

	pub(crate) fn selector_ref(&self) -> &VorbisSelector {
		&self.selector
	}

	/// Consumes the wrapper and returns the underlying packet reader.
	pub(crate) fn into_inner(self) -> PacketReader<T> {
		self.rdr
	}

	/// Scans pages until the selected vorbis bitstream is fully initialised
	/// and its first audio packet has been located.
	///
	/// Scanning does not stop the instant a stream becomes selectable: in
	/// auto mode it keeps going until the first audio packet of the chosen
	/// stream, so that a second, concurrently multiplexed vorbis stream
	/// (whose headers are interleaved with the first one) becomes ready in
	/// time for the ambiguous `MultipleVorbisStreams` error to fire. A
	/// chained successor cannot appear that early, because its first page
	/// follows the end of stream page of its predecessor.
	pub(crate) fn initialise(&mut self) -> Result<(), VorbisError> {
		loop {
			// Already holding the first audio packet of the selected stream.
			if self.primed.is_some() {
				return Ok(());
			}
			let pck = match try!(self.rdr.read_packet()) {
				Some(pck) => pck,
				None => {
					if self.selector.selected_serial().is_some() {
						// A header-only or empty selected stream: legal, the
						// caller will simply get no audio packets.
						return Ok(());
					}
					return Err(VorbisError::NoVorbisStream);
				},
			};
			match try!(self.selector.observe(pck)) {
				ObserveResult::Audio(pck) => {
					self.primed = Some(pck);
					return Ok(());
				},
				ObserveResult::NeedMore => continue,
			}
		}
	}

	/// Returns the next audio packet of the selected bitstream.
	///
	/// Ok(None) marks the end of the physical stream.
	pub(crate) fn next_audio_packet(&mut self)
			-> Result<Option<Packet>, VorbisError> {
		if let Some(pck) = self.primed.take() {
			return Ok(Some(pck));
		}
		loop {
			let pck = match try!(self.rdr.read_packet()) {
				Some(pck) => pck,
				None => return Ok(None),
			};
			match try!(self.selector.observe(pck)) {
				ObserveResult::Audio(pck) => return Ok(Some(pck)),
				ObserveResult::NeedMore => continue,
			}
		}
	}
}

#[cfg(test)]
mod tests;
