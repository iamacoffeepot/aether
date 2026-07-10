use super::*;

// ADR-0104 schedule handler. The cap validates the batch
// synchronously and replies `ScheduleResult` in-handler, then
// pushes one `Schedule` event for the accepted batch. The
// `load_ctx` helper below builds the session-addressed context.

#[test]
fn schedule_happy_path_replies_ok_and_queues_one_event() {
    let (mut cap, queue) = live_cap();
    let (mailer, _rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = load_ctx(&transport);
    let result = AudioCapability::on_schedule(
        &mut cap,
        &mut ctx,
        Schedule {
            events: vec![
                ScheduledEvent {
                    at_millis: 0,
                    event: ScheduledNote::On { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 },
                },
                ScheduledEvent { at_millis: 500, event: ScheduledNote::Off { pitch: 60, instrument_id: 0 } },
            ],
        },
    );
    match result {
        ScheduleResult::Ok { accepted } => assert_eq!(accepted, 2),
        ScheduleResult::Err { error } => panic!("expected Ok, got Err({error})"),
    }
    // The whole batch crosses the queue as exactly one event.
    let event = queue.pop().expect("a schedule event was queued");
    match event {
        AudioEvent::Schedule { events, .. } => assert_eq!(events.len(), 2),
        other => panic!("expected Schedule, got {other:?}"),
    }
    assert!(queue.pop().is_none(), "batch must use a single queue slot");
}

#[test]
fn schedule_empty_batch_replies_err() {
    let (mut cap, queue) = live_cap();
    let (mailer, _rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = load_ctx(&transport);
    let result = AudioCapability::on_schedule(&mut cap, &mut ctx, Schedule { events: vec![] });
    match result {
        ScheduleResult::Err { .. } => {}
        ScheduleResult::Ok { .. } => panic!("empty batch must reject"),
    }
    assert!(queue.pop().is_none(), "rejected batch must not queue an event");
}

#[test]
fn schedule_over_event_cap_rejects_atomically() {
    let (mut cap, queue) = live_cap();
    let (mailer, _rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = load_ctx(&transport);
    let events = (0..=SCHEDULE_MAX_EVENTS)
        .map(|_| ScheduledEvent {
            at_millis: 0,
            event: ScheduledNote::On { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 },
        })
        .collect();
    let result = AudioCapability::on_schedule(&mut cap, &mut ctx, Schedule { events });
    match result {
        ScheduleResult::Err { error } => assert!(error.contains("cap"), "reason: {error}"),
        ScheduleResult::Ok { .. } => panic!("over-cap batch must reject"),
    }
    assert!(queue.pop().is_none(), "over-cap batch must not queue an event");
}

#[test]
fn schedule_over_horizon_rejects_atomically() {
    let (mut cap, queue) = live_cap();
    let (mailer, _rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = load_ctx(&transport);
    let result = AudioCapability::on_schedule(
        &mut cap,
        &mut ctx,
        Schedule {
            events: vec![
                ScheduledEvent {
                    at_millis: 0,
                    event: ScheduledNote::On { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 },
                },
                ScheduledEvent {
                    at_millis: SCHEDULE_MAX_MILLIS + 1,
                    event: ScheduledNote::On { pitch: 64, velocity: 100, instrument_id: 0, pan: 0 },
                },
            ],
        },
    );
    match result {
        ScheduleResult::Err { error } => {
            assert!(error.contains("horizon"), "reason: {error}");
        }
        ScheduleResult::Ok { .. } => panic!("over-horizon batch must reject"),
    }
    // A single bad event rejects the whole batch — the valid event
    // before it never queues.
    assert!(queue.pop().is_none(), "over-horizon batch must reject atomically");
}

#[test]
fn schedule_on_nop_chassis_replies_err() {
    let mut cap = AudioCapabilityState::nop();
    let (mailer, _rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = load_ctx(&transport);
    let result = AudioCapability::on_schedule(
        &mut cap,
        &mut ctx,
        Schedule {
            events: vec![ScheduledEvent {
                at_millis: 0,
                event: ScheduledNote::On { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 },
            }],
        },
    );
    match result {
        ScheduleResult::Err { .. } => {}
        ScheduleResult::Ok { .. } => panic!("nop chassis must reply Err"),
    }
}

// `on_set_master_gain` / `on_set_reverb_send` — both a synchronous
// clamp-and-reply handler over a scalar. No prior coverage existed
// for the master-gain handler's nop-chassis `Err` / clamp behavior;
// backfilled alongside the new reverb-send handler.

#[test]
fn set_master_gain_on_nop_chassis_replies_err() {
    let mut cap = AudioCapabilityState::nop();
    let (mailer, _rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = load_ctx(&transport);
    let result = AudioCapability::on_set_master_gain(&mut cap, &mut ctx, SetMasterGain { gain: 0.5 });
    match result {
        SetMasterGainResult::Err { .. } => {}
        SetMasterGainResult::Ok { .. } => panic!("nop chassis must reply Err"),
    }
}

#[test]
fn set_master_gain_clamps_over_range_input() {
    let (mut cap, queue) = live_cap();
    let (mailer, _rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = load_ctx(&transport);
    let result = AudioCapability::on_set_master_gain(&mut cap, &mut ctx, SetMasterGain { gain: 1.5 });
    match result {
        SetMasterGainResult::Ok { applied_gain } => assert_eq!(applied_gain, 1.0),
        SetMasterGainResult::Err { error } => panic!("expected Ok, got Err({error})"),
    }
    let event = queue.pop().expect("a set_master_gain event was queued");
    assert!(
        matches!(event, AudioEvent::SetMasterGain { gain } if gain == 1.0),
        "expected SetMasterGain clamped to 1.0, got {event:?}",
    );
}

#[test]
fn set_reverb_send_on_nop_chassis_replies_err() {
    let mut cap = AudioCapabilityState::nop();
    let (mailer, _rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = load_ctx(&transport);
    let result = AudioCapability::on_set_reverb_send(&mut cap, &mut ctx, SetReverbSend { send: 0.5 });
    match result {
        SetReverbSendResult::Err { .. } => {}
        SetReverbSendResult::Ok { .. } => panic!("nop chassis must reply Err"),
    }
}

#[test]
fn set_reverb_send_clamps_over_range_input() {
    let (mut cap, queue) = live_cap();
    let (mailer, _rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = load_ctx(&transport);
    let result = AudioCapability::on_set_reverb_send(&mut cap, &mut ctx, SetReverbSend { send: 1.5 });
    match result {
        SetReverbSendResult::Ok { applied_send } => assert_eq!(applied_send, 1.0),
        SetReverbSendResult::Err { error } => panic!("expected Ok, got Err({error})"),
    }
    let event = queue.pop().expect("a set_reverb_send event was queued");
    assert!(
        matches!(event, AudioEvent::SetReverbSend { send } if send == 1.0),
        "expected SetReverbSend clamped to 1.0, got {event:?}",
    );
}
