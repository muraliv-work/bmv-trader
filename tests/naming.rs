//! Checks the names `docs/NAMING.md` can check by reading the repo (DECISION S44). A name this
//! rejects is renamed, not exempted; a new kind of name gets a rule in NAMING.md first.

use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// lowercase-kebab: words of a-z and 0-9, joined by single hyphens.
fn kebab(s: &str) -> bool {
    !s.is_empty()
        && s.split('-').all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// snake_case: words of a-z and 0-9, joined by single underscores.
fn snake(s: &str) -> bool {
    !s.is_empty()
        && s.split('_').all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn names_in(dir: &str, ext: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(root().join(dir))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(ext))
        .collect();
    v.sort();
    v
}

#[test]
fn helpers_mean_what_they_say() {
    assert!(kebab("free-key-add") && kebab("desk-wave1") && !kebab("Free-keys") && !kebab("a--b"));
    assert!(snake("tier2_spend") && !snake("tier2-spend") && !snake("Tier2"));
}

#[test]
fn scripts_are_kebab() {
    for n in names_in("scripts", ".sh") {
        assert!(
            kebab(n.trim_end_matches(".sh")),
            "scripts/{n}: scripts are <area>-<verb>.sh"
        );
    }
}

#[test]
fn sudo_scripts_end_in_setup_and_say_so() {
    for n in names_in("scripts", ".sh") {
        let text = std::fs::read_to_string(root().join("scripts").join(&n)).unwrap();
        let needs_root = text.contains("[[ $EUID -eq 0 ]]");
        assert_eq!(
            needs_root,
            n.ends_with("-setup.sh") || n == "clip-close-signup.sh",
            "scripts/{n}: a script that must run as root ends in -setup.sh (and only those do)"
        );
        if needs_root {
            assert!(
                text.lines().take(10).any(|l| l.contains("sudo")),
                "scripts/{n}: show its sudo command in the header comment (first 10 lines)"
            );
        }
    }
}

#[test]
fn owner_jobs_are_named_for_their_label() {
    for n in names_in("deploy", ".plist") {
        let label = n.trim_end_matches(".plist");
        let job = label
            .strip_prefix("com.bmv-trader.")
            .unwrap_or_else(|| panic!("deploy/{n}: owner jobs are com.bmv-trader.<job>"));
        assert!(
            !job.is_empty() && job.bytes().all(|b| b.is_ascii_lowercase()),
            "deploy/{n}: the job is one lowercase word"
        );
        let text = std::fs::read_to_string(root().join("deploy").join(&n)).unwrap();
        assert!(
            text.contains(&format!("<string>{label}</string>")),
            "deploy/{n}: its Label must be {label}"
        );
    }
}

#[test]
fn binaries_are_crate_or_crate_dash_role() {
    for n in names_in("crates/desk/src/bin", ".rs") {
        let b = n.trim_end_matches(".rs");
        assert!(
            b == "desk" || (b.starts_with("desk-") && kebab(b)),
            "crates/desk/src/bin/{n}"
        );
    }
}

#[test]
fn engine_subcommands_are_kebab() {
    let main = std::fs::read_to_string(root().join("src/main.rs")).unwrap();
    let mut seen = 0;
    for line in main.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("Some(\"")
            && let Some((cmd, tail)) = rest.split_once('"')
            && tail.starts_with(") =>")
            && !cmd.bytes().all(|b| b.is_ascii_digit())
        {
            assert!(kebab(cmd), "src/main.rs: subcommand {cmd:?} must be kebab");
            seen += 1;
        }
    }
    assert!(
        seen > 20,
        "found only {seen} subcommands; has main.rs's dispatch changed shape?"
    );
}

#[test]
fn config_files_follow_the_id_and_field_rules() {
    let read = |p: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(root().join(p)).unwrap()).unwrap()
    };
    let fields = |where_: &str, v: &serde_json::Value| {
        for k in v.as_object().unwrap().keys() {
            assert!(snake(k), "{where_}: field {k:?} must be snake_case");
        }
    };
    let agents = read("deploy/agents.json");
    for (id, a) in agents["agents"].as_object().unwrap() {
        let (role, n) = id.rsplit_once('-').unwrap_or(("", ""));
        assert!(
            kebab(role) && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
            "agent id {id}: <role>-<n>"
        );
        fields(&format!("agents.json, {id}"), a);
        let user = a["runs_as"].as_str().unwrap();
        assert!(
            user.len() <= 12
                && user.starts_with("bmv")
                && user.bytes().all(|b| b.is_ascii_lowercase()),
            "{id}: user {user}"
        );
    }
    let models = read("deploy/models.json");
    fields("models.json", &models);
    for section in ["agents", "engine"] {
        for (id, m) in models[section].as_object().unwrap() {
            assert!(kebab(id) || snake(id), "models.json, {section}: {id}");
            fields(&format!("models.json, {section}, {id}"), m);
        }
    }
    fields("models.json, keys", &models["keys"]);
    for (name, p) in models["free_keys"]["providers"].as_object().unwrap() {
        assert!(kebab(name), "models.json, free_keys: provider {name}");
        let conn = p["connection"].as_str().unwrap();
        assert!(
            kebab(conn) && conn.starts_with(&format!("{name}-")),
            "{name}: connection {conn} is <provider>-<tier>"
        );
    }
}

#[test]
fn the_key_tracker_ids_are_kebab() {
    let keys = std::fs::read_to_string(root().join("src/keys.rs")).unwrap();
    for line in keys.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("id: \"") {
            let id = rest.split('"').next().unwrap();
            assert!(
                kebab(id),
                "src/keys.rs: key id {id:?} is <provider>-<scope>"
            );
        }
    }
}

#[test]
fn console_routes_are_kebab_segments() {
    let ui = std::fs::read_to_string(root().join("src/ui.rs")).unwrap();
    let table = ui
        .split_once("pub const ROUTES")
        .and_then(|(_, rest)| rest.split_once("];"))
        .map(|(t, _)| t)
        .expect("src/ui.rs: no ROUTES table");
    let mut seen = 0;
    for line in table.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("(\"")
            && let Some((_, path)) = rest.split_once("\", \"")
        {
            let path = path.trim_end_matches("\"),");
            for seg in path
                .trim_start_matches('/')
                .split('/')
                .filter(|s| !s.is_empty())
            {
                let seg = seg
                    .strip_suffix(".js")
                    .or(seg.strip_suffix(".css"))
                    .unwrap_or(seg);
                assert!(kebab(seg), "src/ui.rs: route {path:?}: {seg:?} isn't kebab");
            }
            seen += 1;
        }
    }
    assert!(
        seen > 10,
        "found only {seen} routes; has ROUTES changed shape?"
    );
}
