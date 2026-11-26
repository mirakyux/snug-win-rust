// This file runs beside the production EDID module and the extracted production
// context/IPC/callbacks. Only the native WDF boundary is mocked by the harness.
fn dtd_fields(bytes: &[u8]) -> [u64; 10] {
    let d = &bytes[54..72];
    [
        u64::from(u16::from_le_bytes([d[0], d[1]])) * 10_000,
        u64::from(d[2]) | (u64::from(d[4] >> 4) << 8),
        u64::from(d[3]) | (u64::from(d[4] & 15) << 8),
        u64::from(d[5]) | (u64::from(d[7] >> 4) << 8),
        u64::from(d[6]) | (u64::from(d[7] & 15) << 8),
        u64::from(d[8]) | (u64::from(d[11] >> 6) << 8),
        u64::from(d[9]) | (u64::from((d[11] >> 4) & 3) << 8),
        u64::from(d[10] >> 4) | (u64::from((d[11] >> 2) & 3) << 4),
        u64::from(d[10] & 15) | (u64::from(d[11] & 3) << 4),
        u64::from(d[17]),
    ]
}

fn assert_edid_integrity(bytes: &[u8], serial: u32) {
    let baseline = Edid::generate_with(serial);
    assert_eq!(bytes.len(), 128);
    assert_eq!(Edid::get_serial(bytes).unwrap(), serial);
    assert_eq!(&bytes[12..16], &serial.to_le_bytes());
    assert_eq!(bytes.iter().map(|b| u32::from(*b)).sum::<u32>() % 256, 0);
    for i in 0..128 {
        if !(54..72).contains(&i) && i != 81 && i != 127 {
            assert_eq!(bytes[i], baseline[i], "unexpected EDID change at {i}");
        }
    }
    let [clock, h, hb, v, vb, ..] = dtd_fields(bytes);
    assert!(u64::from(bytes[81]) * 10_000_000 >= clock);
    assert!(bytes[81] >= baseline[81]);
    assert!(clock >= 15_000 * (h + hb) && clock <= 255_000 * (h + hb));
    assert!(clock >= 23 * (h + hb) * (v + vb) && clock <= 240 * (h + hb) * (v + vb));
}

#[test]
fn edid_known_timings_are_byte_identical_to_hardware_validated_candidate() {
    for serial in [0, 1, 15, 0x1234_5678, u32::MAX] {
        for (width, height, expected) in [
            (
                1280,
                720,
                [
                    0x01, 0x1d, 0x00, 0x72, 0x51, 0xd0, 0x1e, 0x20, 0x6e, 0x28, 0x55, 0, 0, 0, 0,
                    0, 0, 0x1e,
                ],
            ),
            (
                1600,
                900,
                [
                    0x30, 0x2a, 0x40, 0xc8, 0x60, 0x84, 0x64, 0x30, 0x18, 0x50, 0x13, 0, 0, 0, 0,
                    0, 0, 0x1e,
                ],
            ),
        ] {
            let bytes = Edid::generate_for_mode(serial, width, height, 60);
            assert_edid_integrity(&bytes, serial);
            assert_eq!(&bytes[54..72], &expected);
        }
    }
}

#[test]
fn edid_synthetic_timing_roundtrips_common_portrait_custom_and_field_boundaries() {
    for (width, height, rate) in [
        (1920, 1080, 60),
        (1920, 1080, 120),
        (2560, 1440, 144),
        (3840, 2160, 60),
        (720, 1280, 60),
        (1080, 1920, 60),
        (1366, 768, 59),
        (320, 240, 240),
        (1440, 2560, 60),
        (4095, 4095, 37),
        (1, 4095, 60),
    ] {
        let bytes = Edid::generate_for_mode(7, width, height, rate);
        assert_edid_integrity(&bytes, 7);
        let [clock, h, hb, v, vb, hf, hs, vf, vs, flags] = dtd_fields(&bytes);
        assert_eq!(
            (h, v),
            (u64::from(width), u64::from(height)),
            "{width}x{height}@{rate}"
        );
        assert!(clock > 0 && clock <= 655_350_000);
        assert!(hb > hf + hs && hf > 0 && hs > 0);
        assert!(vb > vf + vs && vf > 0 && vs > 0);
        assert_eq!(flags, 0x1e);
        let pixels = (h + hb) * (v + vb);
        let target = pixels * u64::from(rate);
        let error = clock.abs_diff(target);
        assert!(error <= 5_000, "nearest 10kHz clock");
        assert!(error * 1000 <= target, "relative error <= 0.1%");
        assert!(error * 10 <= pixels, "absolute refresh error <= 0.1Hz");
    }
}

#[test]
fn edid_unrepresentable_or_invalid_input_returns_exact_original() {
    for serial in [0, 15, u32::MAX] {
        for (width, height, rate) in [
            (0, 1080, 60),
            (1920, 0, 60),
            (1920, 1080, 0),
            (1920, 1080, 241),
            (u32::MAX, u32::MAX, u32::MAX),
            (1, 1, 1),
            (1, 1, 60),
            (4096, 2160, 60),
            (7680, 4320, 240),
            (16384, 16384, 240),
            (4095, 4095, 38),
            (3840, 2160, 240),
            (4095, 4095, 1),
            (4095, 1, 60),
            (1280, 720, 22),
            (4095, 4095, 60),
        ] {
            assert_eq!(
                Edid::generate_for_mode(serial, width, height, rate),
                Edid::generate_with(serial),
                "{width}x{height}@{rate}"
            );
        }
    }
    assert!(Edid::get_serial(&[0; 127]).is_err());
    assert!(Edid::get_serial(&[0; 129]).is_err());
}

#[test]
fn edid_only_expands_range_max_clock_when_the_preferred_dtd_needs_it() {
    for (width, height, expected_clock, expected_limit) in [
        (1280, 720, 74_250_000, 15),
        (1600, 900, 108_000_000, 15),
        (1440, 2560, 250_080_000, 26),
        (3840, 2160, 529_200_000, 53),
    ] {
        let bytes = Edid::generate_for_mode(1, width, height, 60);
        assert_edid_integrity(&bytes, 1);
        assert_eq!(dtd_fields(&bytes)[0], expected_clock);
        assert_eq!(bytes[81], expected_limit);
        assert_eq!(&bytes[72..81], &Edid::generate_with(1)[72..81]);
        assert_eq!(&bytes[82..127], &Edid::generate_with(1)[82..127]);
    }
}

#[test]
fn edid_legal_contract_sweep_never_truncates_active_pixels_or_bad_clock() {
    for width in [
        1, 2, 255, 256, 1023, 1024, 1280, 1600, 4094, 4095, 4096, 7680, 16384,
    ] {
        for height in [1, 2, 255, 256, 720, 900, 4095, 4096, 4320, 16384] {
            for rate in [1, 23, 59, 60, 120, 144, 240] {
                let bytes = Edid::generate_for_mode(3, width, height, rate);
                assert_edid_integrity(&bytes, 3);
                if bytes == Edid::generate_with(3) {
                    continue;
                }
                let [clock, h, hb, v, vb, hf, hs, vf, vs, _] = dtd_fields(&bytes);
                assert_eq!((h, v), (u64::from(width), u64::from(height)));
                assert!(hb > hf + hs && hf > 0 && hs > 0);
                assert!(vb > vf + vs && vf > 0 && vs > 0);
                let pixels = (h + hb) * (v + vb);
                let target = pixels * u64::from(rate);
                let error = clock.abs_diff(target);
                assert!(clock != 0 && error * 1000 <= target && error * 10 <= pixels);
            }
        }
    }
}

#[test]
fn real_native_create_uses_first_mode_and_first_rate_without_narrowing_modes() {
    let mut ctx = reset();
    let mut first = data();
    first.modes = vec![
        Mode {
            width: 1280,
            height: 720,
            refresh_rates: vec![60, 120, 240],
        },
        Mode {
            width: 1600,
            height: 900,
            refresh_rates: vec![75, 60],
        },
        Mode {
            width: 16384,
            height: 16384,
            refresh_rates: vec![1, 240],
        },
    ];
    MONITOR_MODES.lock().unwrap()[0].data = first.clone();
    ctx.create_monitor(1).unwrap();
    assert_eq!(monitor_state(), vec![first]);
    assert_eq!(
        EDID_CAPTURES.lock().unwrap().as_slice(),
        &[(1, Edid::generate_for_mode(1, 1280, 720, 60))]
    );
    assert_eq!(LIVE.lock().unwrap().len(), 1);
    assert_eq!(ARRIVAL_ATTEMPTS.lock().unwrap().as_slice(), &[1]);
}

#[test]
fn real_native_preference_preserves_requested_order_instead_of_sorting() {
    for (width, height, rates) in [
        (1280, 720, vec![120, 60]),
        (1600, 900, vec![75, 60]),
        (1440, 2560, vec![60, 90]),
    ] {
        let mut ctx = reset();
        let modes = vec![
            Mode {
                width,
                height,
                refresh_rates: rates.clone(),
            },
            Mode {
                width: 640,
                height: 480,
                refresh_rates: vec![60],
            },
        ];
        MONITOR_MODES.lock().unwrap()[0].data.modes = modes.clone();
        ctx.create_monitor(1).unwrap();
        let captured = EDID_CAPTURES.lock().unwrap()[0].1.clone();
        assert_eq!(
            captured,
            Edid::generate_for_mode(1, width, height, rates[0])
        );
        let fields = dtd_fields(&captured);
        assert_eq!(
            (fields[1], fields[3]),
            (u64::from(width), u64::from(height))
        );
        assert_ne!(
            captured,
            Edid::generate_for_mode(1, width, height, rates[1])
        );
        assert_eq!(monitor_state()[0].modes, modes);
    }
}

fn assert_real_callback_modes(expected: &Monitor) {
    let monitor = MONITOR_MODES.lock().unwrap()[0].object.unwrap().as_ptr();
    let expected: Vec<_> = expected
        .modes
        .iter()
        .flat_map(|mode| {
            mode.refresh_rates
                .iter()
                .map(move |rate| (mode.width, mode.height, *rate))
        })
        .collect();
    let mut descriptions = vec![IDDCX_MONITOR_MODE::default(); expected.len()];
    let mut description_out = IDARG_OUT_PARSEMONITORDESCRIPTION::default();
    assert_eq!(
        parse_modes(
            descriptions.len() as u32,
            descriptions.as_mut_ptr(),
            &mut description_out
        ),
        NTSTATUS::STATUS_SUCCESS
    );
    assert_eq!(
        description_out.MonitorModeBufferOutputCount as usize,
        expected.len()
    );
    assert_eq!(description_out.PreferredMonitorModeIdx, 0);
    let mut targets = vec![IDDCX_TARGET_MODE::default(); expected.len()];
    let mut target_out = IDARG_OUT_QUERYTARGETMODES::default();
    let args = IDARG_IN_QUERYTARGETMODES {
        TargetModeBufferInputCount: targets.len() as u32,
        pTargetModes: targets.as_mut_ptr(),
    };
    assert_eq!(
        monitor_query_modes(monitor, &args, &mut target_out),
        NTSTATUS::STATUS_SUCCESS
    );
    assert_eq!(
        target_out.TargetModeBufferOutputCount as usize,
        expected.len()
    );
    for ((description, target), (width, height, rate)) in
        descriptions.iter().zip(&targets).zip(expected)
    {
        for signal in [
            &description.MonitorVideoSignalInfo,
            &target.TargetVideoSignalInfo.targetVideoSignalInfo,
        ] {
            assert_eq!(
                (signal.activeSize.cx, signal.activeSize.cy),
                (width, height)
            );
            assert_eq!(
                signal.pixelRate,
                u64::from(width) * u64::from(height) * u64::from(rate)
            );
        }
    }
}

#[test]
fn real_native_fallback_never_selects_later_mode_or_rate() {
    for modes in [
        vec![],
        vec![Mode {
            width: 1280,
            height: 720,
            refresh_rates: vec![],
        }],
        vec![
            Mode {
                width: 7680,
                height: 4320,
                refresh_rates: vec![240, 60],
            },
            Mode {
                width: 1280,
                height: 720,
                refresh_rates: vec![60],
            },
        ],
        vec![Mode {
            width: 3840,
            height: 2160,
            refresh_rates: vec![240, 60],
        }],
    ] {
        let mut ctx = reset();
        MONITOR_MODES.lock().unwrap()[0].data.modes = modes.clone();
        ctx.create_monitor(1).unwrap();
        assert_eq!(
            EDID_CAPTURES.lock().unwrap().as_slice(),
            &[(1, Edid::generate_with(1))]
        );
        assert_eq!(monitor_state()[0].modes, modes);
    }
}

#[test]
fn real_owned_ipc_multimode_create_replace_release_uses_actual_edid() {
    reset_owned();
    let mut first = data();
    first.modes = vec![
        Mode {
            width: 1280,
            height: 720,
            refresh_rates: vec![60, 120],
        },
        Mode {
            width: 1920,
            height: 1080,
            refresh_rates: vec![60, 144],
        },
    ];
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
    assert_eq!(
        snug_create(OTHER_OWNER, vec![first.clone()]),
        SnugUpdateResult::NotFound
    );
    assert!(EDID_CAPTURES.lock().unwrap().is_empty());
    assert_eq!(
        snug_create(OWNER, vec![first.clone()]),
        SnugUpdateResult::Applied
    );
    assert_eq!(monitor_state(), vec![first.clone()]);
    assert_real_callback_modes(&first);
    let mut second = first.clone();
    second.modes[0] = Mode {
        width: 1600,
        height: 900,
        refresh_rates: vec![60, 120],
    };
    assert_eq!(
        snug_replace(OTHER_OWNER, vec![second.clone()]),
        SnugUpdateResult::NotFound
    );
    assert_eq!(EDID_CAPTURES.lock().unwrap().len(), 1);
    assert_eq!(
        snug_replace(OWNER, vec![second.clone()]),
        SnugUpdateResult::Applied
    );
    assert_eq!(monitor_state(), vec![second.clone()]);
    assert_real_callback_modes(&second);
    // Legal requests that cannot fit an EDID DTD still traverse native creation.
    second.modes[0] = Mode {
        width: 7680,
        height: 4320,
        refresh_rates: vec![240, 60],
    };
    assert_eq!(
        snug_replace(OWNER, vec![second.clone()]),
        SnugUpdateResult::Applied
    );
    assert_eq!(monitor_state(), vec![second.clone()]);
    assert_real_callback_modes(&second);
    assert_eq!(
        EDID_CAPTURES.lock().unwrap().as_slice(),
        &[
            (1, Edid::generate_for_mode(1, 1280, 720, 60)),
            (1, Edid::generate_for_mode(1, 1600, 900, 60)),
            (1, Edid::generate_with(1)),
        ]
    );
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::Applied);
    assert!(monitor_state().is_empty());
    assert!(LIVE.lock().unwrap().is_empty());
}

#[test]
fn real_ipc_validation_still_rejects_invalid_contract_before_native_calls() {
    for (width, height, rates) in [
        (0, 720, vec![60]),
        (16385, 720, vec![60]),
        (1280, 0, vec![60]),
        (1280, 16385, vec![60]),
        (1280, 720, vec![0]),
        (1280, 720, vec![241]),
        (1280, 720, vec![]),
    ] {
        reset_owned();
        let mut monitor = data();
        monitor.modes = vec![Mode {
            width,
            height,
            refresh_rates: rates,
        }];
        assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
        assert_eq!(
            snug_create(OWNER, vec![monitor]),
            SnugUpdateResult::InvalidData
        );
        assert!(EDID_CAPTURES.lock().unwrap().is_empty());
        assert!(LIVE.lock().unwrap().is_empty());
    }
}
