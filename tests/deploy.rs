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
    assert!(plist.contains("scripts/reconcile.sh") && !plist.contains("bmv-trial10"));
}
