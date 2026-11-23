//! The control stream's inbound messages: [`ControlTask::on_frame`] decodes a frame by its
//! type and hands it to that message's method. Each answers on the control stream or hands the
//! change to the data plane, and returns whether the task goes on.

use super::*;

impl ControlTask {
    pub(super) async fn on_frame(&mut self, frame: std::io::Result<(u64, Vec<u8>)>) -> Step {
        use v2msg::decode;
        let (ty, body) = frame?;
        if let Some(ev) = v2msg::decode_input_event(ty, &body) {
            self.on_input_edge(ev).await
        } else if let Ok(req) = decode::<Reconfigure>(ty, &body) {
            self.on_reconfigure(req).await
        } else if let Ok(id) = decode::<punktfunk_core::quic::PadIdentity>(ty, &body) {
            self.on_pad_identity(id)
        } else if let Ok(req) = decode::<v2msg::Screens>(ty, &body) {
            self.on_screens(req)
        } else if let Ok(req) = decode::<SetBitrate>(ty, &body) {
            self.on_set_bitrate(req).await
        } else if let Ok(ack) = decode::<punktfunk_core::quic::ShardPayloadAck>(ty, &body) {
            self.on_shard_ack(ack)
        } else if let Ok(req) = decode::<ProbeShaped>(ty, &body) {
            self.on_probe(req).await
        } else if let Ok(probe) = decode::<ClockProbe>(ty, &body) {
            self.on_clock_probe(probe).await
        } else if let Ok(m) = decode::<punktfunk_core::quic::CursorRenderMode>(ty, &body) {
            self.on_cursor_mode(m)
        } else if let Ok(ctl) = decode::<ClipControl>(ty, &body) {
            self.on_clip_control(ctl).await
        } else if let Ok(offer) = decode::<ClipOffer>(ty, &body) {
            self.on_clip_offer(offer)
        } else if punktfunk_core::quic::v2::dgram::RetiredFeedback::reads(ty) {
            self.on_retired_feedback(ty, &body)
        } else {
            tracing::debug!(ty, "control frame of a type this host does not read");
            Ok(())
        }
    }

    /// A key edge off the control stream, into the datagram plane's queue ([`offer_edge`]).
    async fn on_input_edge(&mut self, ev: InputEvent) -> Step {
        let task = &self.task;
        offer_edge(
            ev,
            &task.session_grants,
            &task.input_tx,
            &task.counters,
            &mut self.denied,
        )
        .await;
        Ok(())
    }

    /// A codec ask: the client cannot decode `exclude`. The stream thread rebuilds when the
    /// session may change its capture, another codec is left and no ask is pending; otherwise
    /// the ask is refused at once.
    async fn on_codec_ask(&mut self, exclude: u8) -> Step {
        let mut to = None;
        if self.task.format_change_ok && !self.task.held() && self.asked == 0 {
            self.excluded |= exclude;
            let left = self.task.client_codecs & !self.excluded;
            let t = &self.task;
            to = next_codec(t.codec, left, t.host_codecs, t.preferred_codec);
        }
        match to {
            Some(to) if self.task.ends.format_tx.send(to).is_ok() => {
                self.asked = exclude;
                tracing::info!(from = ?self.task.codec, ?to, "codec change accepted — rebuilding");
                Ok(())
            }
            _ => {
                tracing::info!(codec = ?self.task.codec, excluded = exclude,
                    "codec change refused (no other codec, PyroWave, a shared \
                     capture, or one already pending)");
                let ack = Reconfigured {
                    accepted: false,
                    mode: self.held.mode(),
                    exclude_codecs: exclude,
                };
                self.send(&ack).await
            }
        }
    }

    /// A `Reconfigure`, as [`reconfigure_ask`] reads it: a codec ask goes on as one; a mode
    /// switch is acked at once with the mode the client now holds, then handed to the stream
    /// thread with its view. A refused switch hands nothing on; a joiner's is refused while its
    /// display is held.
    async fn on_reconfigure(&mut self, req: Reconfigure) -> Step {
        let now = std::time::Instant::now();
        let too_soon = self
            .last_accepted_switch
            .is_some_and(|t| now.duration_since(t) < MIN_SWITCH_INTERVAL);
        let exact = self.task.exact_rate_ok;
        let req = Reconfigure {
            mode: crate::native::handshake::deliverable_rate(req.mode, exact),
            ..req
        };
        let t = &self.task;
        let live = t.live_reconfig_ok && !t.held();
        let accepted = match reconfigure_ask(&req, &self.held, t.codec, live, too_soon) {
            Ask::Codec(exclude) => return self.on_codec_ask(exclude).await,
            Ask::Mode(asked) => Some(asked),
            Ask::Refused => None,
        };
        if let Some(asked) = accepted {
            (self.held, self.asked_at) = (asked, Some(self.told_epoch));
            self.last_accepted_switch = Some(now);
        }
        let ack = Reconfigured {
            accepted: accepted.is_some(),
            mode: self.held.mode(),
            exclude_codecs: 0,
        };
        self.send(&ack).await?;
        if let Some(asked) = accepted {
            self.task.ends.reconfig_tx.send(asked)?;
        }
        Ok(())
    }

    /// Which player a pad is, for the input thread, under the gamepad grant.
    fn on_pad_identity(&mut self, id: punktfunk_core::quic::PadIdentity) -> Step {
        if self.task.session_grants.load(Ordering::Relaxed) & punktfunk_core::quic::GRANT_GAMEPAD
            != 0
            && self
                .task
                .input_tx
                .try_send(super::super::input::ClientInput::PadIdentity(id))
                .is_err()
        {
            tracing::warn!("pad identity dropped: input queue full");
        }
        Ok(())
    }

    /// A layout ask for the stream's screen set. A session without one has dropped the
    /// receiver: the ask is ignored and the task goes on.
    fn on_screens(&mut self, req: v2msg::Screens) -> Step {
        tracing::info!(screens = ?req.screens, "client screens");
        if self.task.ends.screens_tx.send(req.screens).is_err()
            && !std::mem::replace(&mut self.closed.screens_ask, true)
        {
            tracing::debug!("screens ask ignored: this session has no screen set");
        }
        Ok(())
    }

    /// A rate ask, answered by [`rate::resolve`] and handed to the stream thread, which retargets
    /// the encoder in place where it can, else rebuilds it.
    async fn on_set_bitrate(&mut self, req: SetBitrate) -> Step {
        let shared = &self.task.shared;
        let live = shared.live_bitrate.load(Ordering::Relaxed);
        self.link.note_bitrate_ask(req.bitrate_kbps, live);
        let facts = rate::RateFacts {
            pyrowave: self.task.codec == crate::encode::Codec::PyroWave,
            ramp_open: shared.ramp_open.load(Ordering::SeqCst),
            live_kbps: live,
            cadence_degraded: shared.cadence_degraded.load(Ordering::Relaxed),
            behind_score: shared.cadence_behind_score.load(Ordering::Relaxed),
            share_kbps: self.task.counters.share.share_kbps(),
        };
        let ceiling = &shared.encoder_ceiling;
        let (want, why) = rate::resolve(req.bitrate_kbps, &facts, &mut self.pyrowave_pin, |w| {
            ceiling.lock().unwrap_or_else(|e| e.into_inner()).resolve(w)
        });
        tracing::debug!(
            requested_kbps = req.bitrate_kbps,
            resolved_kbps = want,
            "mid-stream bitrate change requested"
        );
        let ack = bitrate_ack(want, why, self.task.ack_reason);
        self.send(&ack).await?;
        self.task.ends.bitrate_tx.send(want)?;
        Ok(())
    }

    /// The grow gate: the packetizer may exceed the old size only after this ack. A full queue
    /// or an ended watcher drops it, unlogged, so a client repeating acks costs nothing.
    fn on_shard_ack(&mut self, ack: punktfunk_core::quic::ShardPayloadAck) -> Step {
        if self
            .task
            .ends
            .shard_ack_tx
            .try_send(ack.shard_payload)
            .is_ok()
        {
            tracing::info!(
                shard_payload = ack.shard_payload,
                "client acked shard-payload change"
            );
        }
        Ok(())
    }

    /// A speed-test probe, never past what the ports carry: what a burst learns past them is
    /// loss. Spaced ([`ProbeSpacing`]) unless the session is a diagnostic one.
    async fn on_probe(&mut self, mut req: ProbeShaped) -> Step {
        req.target_kbps = probe_bound(req.target_kbps, self.task.shared.ports);
        let open = self.task.shared.ramp_open.load(Ordering::SeqCst);
        let now = std::time::Instant::now();
        if !self.task.probe_only && !self.probe_spacing.admit(now, is_ramp_length(&req), open) {
            tracing::warn!(
                target_kbps = req.target_kbps,
                "speed-test probe rejected (rate-limited)"
            );
            // The client holds its reports until a probe is answered.
            return self.send(&super::super::stream::declined()).await;
        }
        tracing::info!(
            target_kbps = req.target_kbps,
            duration_ms = req.duration_ms,
            "speed-test probe requested"
        );
        self.task.ends.probe_tx.send(req)?;
        Ok(())
    }

    /// Echoed here, with t2 and t3 in the session clock video pts leave in: no data-plane hop.
    async fn on_clock_probe(&mut self, probe: ClockProbe) -> Step {
        let t2_ns = self.task.clock.to_wire(now_ns());
        let echo = ClockEcho {
            t1_ns: probe.t1_ns,
            t2_ns,
            t3_ns: self.task.clock.to_wire(now_ns()),
        };
        self.send(&echo).await
    }

    /// The data plane edge-detects it per tick (forward and exclude, or composite). Inert
    /// without the cursor cap.
    fn on_cursor_mode(&mut self, m: punktfunk_core::quic::CursorRenderMode) -> Step {
        self.task
            .shared
            .cursor_client_draws
            .store(m.client_draws, Ordering::Relaxed);
        tracing::info!(
            client_draws = m.client_draws,
            "cursor render mode set by client"
        );
        Ok(())
    }

    /// Clipboard sync on or off, under the operator policy, the live grant and the backend
    /// ([`resolve_clip_control`]), answered with the state that resolved.
    async fn on_clip_control(&mut self, ctl: ClipControl) -> Step {
        let granted = self.task.session_grants.load(Ordering::Relaxed)
            & punktfunk_core::quic::GRANT_CLIPBOARD
            != 0;
        let (enabled, resolved_policy, reason) = resolve_clip_control(
            pf_clipboard::policy(),
            granted,
            self.task.clip.available,
            ctl,
        );
        self.task.clip_enabled.store(enabled, Ordering::SeqCst);
        // Enable re-announces the host clipboard; disable drops our selection. Inert handle:
        // a dropped send is fine.
        let _ = self
            .task
            .clip
            .cmd_tx
            .send(ClipCoordCmd::SetEnabled(enabled));
        tracing::info!(
            enabled,
            files = enabled && resolved_policy & punktfunk_core::quic::CLIP_POLICY_FILES != 0,
            "clipboard control"
        );
        let state = ClipState {
            enabled,
            policy: resolved_policy,
            reason,
        };
        self.send(&state).await
    }

    /// The WRITE half of CLIPBOARD: a client selection to install on the host clipboard.
    fn on_clip_offer(&mut self, offer: ClipOffer) -> Step {
        if clip_offer_permitted(
            self.task.session_grants.load(Ordering::Relaxed),
            self.task.clip_enabled.load(Ordering::SeqCst),
        ) {
            tracing::debug!(
                seq = offer.seq,
                kinds = offer.kinds.len(),
                "clipboard offer from client"
            );
            let mimes = offer.kinds.iter().map(|k| k.mime.clone()).collect();
            let _ = self.task.clip.cmd_tx.send(ClipCoordCmd::RemoteOffer {
                seq: offer.seq,
                mimes,
            });
        } else {
            self.denied.note(GrantClass::Clipboard);
        }
        Ok(())
    }

    /// An older client's control-stream report, joined to the datagram's feedback.
    fn on_retired_feedback(&mut self, ty: u64, body: &[u8]) -> Step {
        let fb = self.retired.read(ty, body);
        if let (Some(fb), Some(tx)) = (fb, self.task.feedback_tx.upgrade()) {
            let _ = tx.try_send(fb);
        }
        Ok(())
    }
}

/// What a `Reconfigure` asks of the stream thread.
#[derive(Debug, PartialEq)]
enum Ask {
    /// A mode switch, with the view [`Intent::ask`] keeps for it. The same mode with another
    /// view is a switch too.
    ///
    /// [`Intent::ask`]: crate::native::geometry::Intent::ask
    Mode(crate::native::geometry::Intent),
    /// These `CODEC_*` bits off. The mode and view stay as they run.
    Codec(u8),
    /// A mode switch refused.
    Refused,
}

/// What `req` asks of a session that holds `held` and encodes `codec`. A mode switch is
/// refused when the backend cannot `live_reconfig`, the mode cannot land, or the last switch
/// was `too_soon` ([`MIN_SWITCH_INTERVAL`]).
fn reconfigure_ask(
    req: &Reconfigure,
    held: &crate::native::geometry::Intent,
    codec: crate::encode::Codec,
    live_reconfig: bool,
    too_soon: bool,
) -> Ask {
    let mode = req.mode;
    if req.exclude_codecs != 0 {
        return Ask::Codec(req.exclude_codecs);
    }
    // Same bound as the handshake.
    let valid = crate::encode::validate_refresh(mode.refresh_hz).is_ok()
        && crate::encode::validate_dimensions(codec, mode.width, mode.height).is_ok();
    if !live_reconfig {
        // Gamescope, a mirror, per-client-mode identity or a held display: the client scales.
        tracing::info!(
            ?mode,
            "mode switch rejected (backend cannot live-reconfigure)"
        );
        Ask::Refused
    } else if !valid {
        tracing::warn!(?mode, "mode switch rejected (invalid size or refresh rate)");
        Ask::Refused
    } else if too_soon {
        tracing::warn!(?mode, "mode switch rejected (rate-limited)");
        Ask::Refused
    } else {
        let asked = held.ask(mode, req.view, codec);
        tracing::info!(?mode, view = ?asked.view(), "mode switch accepted");
        Ask::Mode(asked)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::native::geometry::Intent;

    use crate::native::geometry::tests::asked;

    /// A mode and the view it is shown at, as the rows spell an intent.
    type ModeAsk = (punktfunk_core::Mode, Option<(u32, u32)>);

    /// [`ControlTask::on_reconfigure`]'s answer to `req` from a task that holds `held`: the ask
    /// it hands the stream thread (`None`: refused, or a codec ask), and what it holds after.
    /// `exact` is the session's exact-rate display, `live` its live reconfigure with no held join.
    pub(in crate::native) fn control_ask(
        req: Reconfigure,
        held: Intent,
        codec: crate::encode::Codec,
        exact: bool,
        live: bool,
    ) -> (Option<Intent>, Intent) {
        let req = Reconfigure {
            mode: crate::native::handshake::deliverable_rate(req.mode, exact),
            ..req
        };
        match reconfigure_ask(&req, &held, codec, live, false) {
            Ask::Mode(asked) => (Some(asked), asked),
            Ask::Codec(_) | Ask::Refused => (None, held),
        }
    }

    use crate::native::handshake::tests::mode;

    /// One `Reconfigure` against what the control task holds, and the ask it hands on.
    struct AskRow {
        name: &'static str,
        held: ModeAsk,
        req: Reconfigure,
        exact: bool,
        live: bool,
        handed: Option<ModeAsk>,
        held_after: ModeAsk,
        verdict: crate::native::tests::Verdict,
    }

    #[rustfmt::skip]
    fn ask_rows() -> Vec<AskRow> {
        use crate::native::tests::Verdict::Wanted;
        let (hd, uhd) = (mode(1920, 1080, 60), mode(3840, 2160, 60));
        let viewed = (uhd, Some((1920, 1080)));
        let req = |mode, view| Reconfigure { mode, exclude_codecs: 0, view };
        let row = |name, held, req, (exact, live), handed, held_after, verdict| AskRow {
            name, held, req, exact, live, handed, held_after, verdict,
        };
        let on = (false, true);
        let uhd120 = mode(3840, 2160, 120);
        let exact = punktfunk_core::Mode::exact(3840, 2160, 59_940);
        vec![
            row("a rate pick at the live size keeps the view in force", viewed,
                req(uhd120, None), on, Some((uhd120, Some((1920, 1080)))),
                (uhd120, Some((1920, 1080))), Wanted),
            row("another size with no view clears it", viewed, req(mode(2560, 1440, 60), None),
                on, Some((mode(2560, 1440, 60), None)), (mode(2560, 1440, 60), None), Wanted),
            row("a view of its own replaces it", viewed, req(uhd, Some((1280, 720))), on,
                Some((uhd, Some((1280, 720)))), (uhd, Some((1280, 720))), Wanted),
            row("a view the mode's size clears it", viewed, req(uhd, Some((3840, 2160))), on,
                Some((uhd, None)), (uhd, None), Wanted),
            row("a view larger than the mode clears it", viewed, req(hd, Some((2560, 1440))), on,
                Some((hd, None)), (hd, None), Wanted),
            row("a fraction on a whole-hertz display asks the whole rate", viewed, req(exact, None),
                on, Some((uhd, Some((1920, 1080)))), (uhd, Some((1920, 1080))), Wanted),
            row("a fraction where the display runs one stays", viewed, req(exact, None),
                (true, true), Some((exact, Some((1920, 1080)))), (exact, Some((1920, 1080))),
                Wanted),
            row("no live reconfigure (gamescope, a mirror, a held join): refused, nothing held \
                 moves", viewed, req(uhd120, None), (false, false), None, viewed, Wanted),
            row("a size no encoder takes: refused", viewed, req(mode(3841, 2160, 60), None), on,
                None, viewed, Wanted),
            row("a codec ask hands no mode on", viewed, Reconfigure {
                    exclude_codecs: punktfunk_core::quic::CODEC_HEVC, ..req(uhd, None) },
                on, None, viewed, Wanted),
            row("after a framed resize on a shared display was corrected, a rate pick at the \
                 told size keeps the told view", (hd, Some((1280, 720))),
                req(mode(1920, 1080, 120), None), on,
                Some((mode(1920, 1080, 120), Some((1280, 720)))),
                (mode(1920, 1080, 120), Some((1280, 720))), Wanted),
        ]
    }

    /// What a control ask hands the stream thread, and the mode and view the task holds after.
    #[test]
    fn geometry_rows() {
        let pair = |i: Intent| (i.mode(), i.view());
        for r in ask_rows() {
            let held = asked(r.held.0, r.held.1);
            let got = control_ask(r.req, held, crate::encode::Codec::H265, r.exact, r.live);
            assert_eq!(
                (got.0.map(pair), pair(got.1)),
                (r.handed, r.held_after),
                "{} ({:?})",
                r.name,
                r.verdict
            );
        }
    }

    /// A rate pick keeps the view in force; a new size, or a view of its own, does not.
    #[test]
    fn a_rate_pick_keeps_the_view() {
        let mode = |width, height, refresh_hz| punktfunk_core::Mode {
            width,
            height,
            refresh_hz,
            refresh_mhz: 0,
        };
        let (live, kept) = (mode(3840, 2160, 60), Some((1920, 1080)));
        let held = asked(live, kept);
        let view_of = |m, view| held.ask(m, view, crate::encode::Codec::H265).view();
        assert_eq!(view_of(mode(3840, 2160, 120), None), kept);
        assert_eq!(view_of(mode(2560, 1440, 60), None), None);
        let own = Some((1280, 720));
        assert_eq!(view_of(live, own), own);
        let cleared = view_of(live, Some((3840, 2160)));
        assert_eq!(cleared, None, "a view the mode's size keeps none of it");
    }

    /// A mode ask hands on its view only while its mode can be scaled to it; a codec ask
    /// leaves the mode and view alone; a refused switch hands nothing on.
    #[test]
    fn a_reconfigure_hands_on_the_view_it_keeps() {
        use crate::encode::Codec::H265;
        let mode = punktfunk_core::Mode {
            width: 2048,
            height: 1152,
            refresh_hz: 60,
            refresh_mhz: 0,
        };
        let req = |view, exclude_codecs| Reconfigure {
            mode,
            exclude_codecs,
            view,
        };
        let held = asked(mode, None);
        let ask = |req: Reconfigure| reconfigure_ask(&req, &held, H265, true, false);
        assert_eq!(
            ask(req(Some((1366, 768)), 0)),
            Ask::Mode(asked(mode, Some((1366, 768))))
        );
        for dropped in [(2048, 1152), (2560, 1440), (1366, 1440)] {
            let got = ask(req(Some(dropped), 0));
            assert_eq!(got, Ask::Mode(asked(mode, None)), "{dropped:?}");
        }
        let hevc = punktfunk_core::quic::CODEC_HEVC;
        assert_eq!(ask(req(Some((1366, 768)), hevc)), Ask::Codec(hevc));
        let viewed = req(Some((1366, 768)), 0);
        let refused = |live, too_soon, r: &Reconfigure| {
            reconfigure_ask(r, &held, H265, live, too_soon) == Ask::Refused
        };
        assert!(refused(false, false, &viewed), "no live reconfigure");
        assert!(refused(true, true, &viewed), "too soon");
        let odd = Reconfigure {
            mode: punktfunk_core::Mode {
                width: 2047,
                ..mode
            },
            ..viewed
        };
        assert!(refused(true, false, &odd), "a mode that cannot land");
    }
}
