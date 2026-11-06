//! launchd schedules are written in Mac time (US Central). A slot written in ET hours by mistake
//! would run an hour late, so the reconcile job's slots are checked against the ET session here.

fn int_after(s: &str, key: &str) -> Option<u32> {
    let rest = &s[s.find(&format!("<key>{key}</key><integer>"))? + key.len() + 20..];
    rest[..rest.find('<')?].parse().ok()
}

#[test]
fn reconcile_runs_every_15_minutes_from_9_00_to_15_45_et() {
    let plist = std::fs::read_to_string("deploy/com.bmv-trader.reconcile.plist").unwrap();
    let slots: Vec<(u32, u32)> = plist
        .lines()
        .filter(|l| l.contains("<key>Hour</key>"))
        .map(|l| {
            (
                int_after(l, "Hour").unwrap() + 1,
                int_after(l, "Minute").unwrap(),
            )
        })
        .collect();
    let et: Vec<(u32, u32)> = (9..=15)
        .flat_map(|h| [0, 15, 30, 45].map(|m| (h, m)))
        .collect();
    assert_eq!(slots, et);
    assert!(plist.contains("<string>desk-ping</string>\n    <string>reconcile</string>"));
    assert!(!plist.contains("bmv-trial10") && !plist.contains(".sh</string>"));
}

#[test]
fn trial_10_runs_the_pinned_binary_through_desk_ping() {
    let plist = std::fs::read_to_string("deploy/com.bmv-trader.desk.plist").unwrap();
    assert!(plist.contains(
        "<string>desk-ping</string>\n    <string>desk-run</string>\n    <string>/usr/local/libexec/bmv-trial10</string>\n    <string>desk-run</string>"
    ));
    assert!(!plist.contains(".sh</string>"));
}

#[test]
fn trial_14_runs_the_pinned_binary_at_9_46_and_16_20_et_on_weekdays() {
    for (job, et) in [("intraday-pick", (9, 46)), ("intraday-score", (16, 20))] {
        let plist = std::fs::read_to_string(format!(
            "deploy/com.bmv-trader.{}.plist",
            job.replace('-', "")
        ))
        .unwrap();
        assert!(plist.contains(&format!(
            "<string>desk-ping</string>\n    <string>{job}</string>\n    <string>/usr/local/libexec/bmv-trial14</string>\n    <string>{job}</string>"
        )));
        let slots: Vec<(u32, u32, u32)> = plist
            .lines()
            .filter(|l| l.contains("<key>Hour</key>"))
            .map(|l| {
                (
                    int_after(l, "Weekday").unwrap(),
                    int_after(l, "Hour").unwrap() + 1,
                    int_after(l, "Minute").unwrap(),
                )
            })
            .collect();
        assert_eq!(
            slots,
            (1..=5).map(|d| (d, et.0, et.1)).collect::<Vec<_>>(),
            "{job}"
        );
        assert!(!plist.contains(".sh</string>"));
    }
}
