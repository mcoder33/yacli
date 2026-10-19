use assert_cmd::Command;
use mockito::{Matcher, Mock, Server, ServerGuard};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, Mutex},
};
use tempfile::{TempDir, tempdir};

const SINGLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:series\r\nSUMMARY:Planning\r\nDTSTART:20261020T073000Z\r\nDTEND:20261020T080000Z\r\nORGANIZER:mailto:owner@example.test\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:guest@example.test\r\nX-CUSTOM:keep\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
const SERIES: &str = include_str!("fixtures/calendar/series.ics");
const HREF: &str = "/cal/team/item.ics";

fn multistatus(properties: &str, href: &str) -> String {
    format!(
        r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>{href}</d:href><d:propstat><d:prop>{properties}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
    )
}
fn data_report(input: &str) -> String {
    multistatus(
        &format!(
            "<d:getetag>\"v1\"</d:getetag><c:calendar-data><![CDATA[{input}]]></c:calendar-data>"
        ),
        HREF,
    )
}

struct Harness {
    config: TempDir,
    server: ServerGuard,
    _discovery: Vec<Mock>,
}
impl Harness {
    fn new(input: &str) -> Self {
        let config = tempdir().unwrap();
        let mut server = Server::new();
        fs::write(
            config.path().join("accounts.toml"),
            format!(
                r#"version = 1
[accounts.mock]
email = "owner@example.test"
default = true
[accounts.mock.mail]
enabled = false
auth_mode = "oauth_xoauth2"
imap_host = "imap.example.test"
imap_port = 993
smtp_host = "smtp.example.test"
smtp_port = 465
[accounts.mock.disk]
enabled = false
auth_mode = "oauth"
rest_base_url = "https://disk.example.test"
[accounts.mock.calendar]
enabled = true
auth_mode = "app_password"
caldav_base_url = "{}"
credential_ref = "store:calendar"
"#,
                server.url()
            ),
        )
        .unwrap();
        fs::write(config.path().join("credentials.toml"), "version = 1\n[accounts.mock.services.calendar]\nkind = \"app_password\"\nsecret = \"synthetic-calendar-password\"\n").unwrap();
        let responses = [
            (
                "/",
                multistatus(
                    "<d:current-user-principal><d:href>/principal/</d:href></d:current-user-principal>",
                    "/",
                ),
            ),
            (
                "/principal/",
                multistatus(
                    "<c:calendar-home-set><d:href>/cal/</d:href></c:calendar-home-set>",
                    "/principal/",
                ),
            ),
            (
                "/cal/",
                multistatus(
                    "<d:displayname>Team</d:displayname><d:resourcetype><d:collection/><c:calendar/></d:resourcetype>",
                    "/cal/team/",
                ),
            ),
        ];
        let mut discovery = Vec::new();
        for (path, body) in responses {
            discovery.push(
                server
                    .mock("PROPFIND", path)
                    .with_status(207)
                    .with_body(body)
                    .create(),
            );
        }
        discovery.push(
            server
                .mock("REPORT", "/cal/team/")
                .match_body(Matcher::Regex("prop-filter name=\"UID\"".into()))
                .with_status(207)
                .with_body(data_report(input))
                .create(),
        );
        Self {
            config,
            server,
            _discovery: discovery,
        }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::cargo_bin("yacli").unwrap();
        cmd.env("YACLI_CONFIG_DIR", self.config.path())
            .env("YACLI_SECRET_BACKEND", "file");
        cmd
    }
    fn read(&mut self, input: &str, times: usize) -> Mock {
        self.server
            .mock("GET", HREF)
            .with_status(200)
            .with_header("ETag", "\"v1\"")
            .with_header("Schedule-Tag", "\"s1\"")
            .with_body(input)
            .expect(times)
            .create()
    }
    fn stateful_read(&mut self, state: Arc<Mutex<String>>, times: usize) -> Mock {
        self.server
            .mock("GET", HREF)
            .with_status(200)
            .with_header("ETag", "\"v1\"")
            .with_header("Schedule-Tag", "\"s1\"")
            .with_body_from_request(move |_| state.lock().unwrap().as_bytes().to_vec())
            .expect(times)
            .create()
    }
    fn put(&mut self, state: Arc<Mutex<String>>, status: usize, times: usize) -> Mock {
        self.server
            .mock("PUT", HREF)
            .match_header("If-Match", "\"v1\"")
            .match_header("If-Schedule-Tag-Match", "\"s1\"")
            .match_header("If-None-Match", Matcher::Missing)
            .with_status(status)
            .with_body_from_request(move |req| {
                *state.lock().unwrap() = req.utf8_lossy_body().unwrap().into_owned();
                Vec::new()
            })
            .expect(times)
            .create()
    }
    fn cli(&self, extra: &[&str]) -> Command {
        let mut cmd = self.command();
        cmd.args([
            "calendar",
            "move",
            "series",
            "--account",
            "mock",
            "--calendar",
            "team",
            "--scope",
            "event",
            "--start",
            "2026-10-27T07:30:00Z",
        ]);
        cmd.args(extra);
        cmd
    }
    fn call(&self, mcp: bool, dry_run: bool, occurrence: bool) -> Value {
        let mut cmd = self.command();
        if mcp {
            let mut args = json!({"account":"mock", "calendar":"team", "uid":"series", "scope": if occurrence { "occurrence" } else { "event" }, "start":"2026-10-27T07:30:00Z", "dry_run":dry_run});
            if occurrence {
                args["occurrence"] = json!("2026-10-20T07:30:00Z");
            }
            let input = [
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"calendar-tests","version":"1"}}}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"yacli.calendar.update","arguments":args}}),
            ].iter().map(|v| v.to_string()+"\n").collect::<String>();
            let output = cmd
                .arg("mcp")
                .write_stdin(input)
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            let response: Value = String::from_utf8(output)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .find(|v| v["id"] == 2)
                .unwrap();
            assert!(response.get("error").is_none(), "{response}");
            response["result"]["structuredContent"].clone()
        } else {
            cmd.args([
                "calendar",
                "move",
                "series",
                "--account",
                "mock",
                "--calendar",
                "team",
                "--scope",
                if occurrence { "occurrence" } else { "event" },
                "--start",
                "2026-10-27T07:30:00Z",
            ]);
            if occurrence {
                cmd.args(["--occurrence", "2026-10-20T07:30:00Z"]);
            }
            if dry_run {
                cmd.arg("--dry-run");
            }
            serde_json::from_slice(&cmd.assert().success().get_output().stdout).unwrap()
        }
    }
}

#[test]
fn cli_and_mcp_preview_do_not_write() {
    for mcp in [false, true] {
        let mut h = Harness::new(SINGLE);
        let get = h.read(SINGLE, 1);
        let state = Arc::new(Mutex::new(SINGLE.to_string()));
        let put = h.put(state.clone(), 204, 0);
        let result = h.call(mcp, true, false);
        assert_eq!(result["account"], "mock");
        assert_eq!(result["calendar"]["id"], "team");
        assert_eq!(result["dry_run"], true);
        assert_eq!(result["write_outcome"], "not_written");
        assert_eq!(result["before"]["start"], "2026-10-20T07:30:00Z");
        assert_eq!(result["after"]["end"], "2026-10-27T08:00:00Z");
        assert_eq!(*state.lock().unwrap(), SINGLE);
        get.assert();
        put.assert();
    }
}

#[test]
fn cli_and_mcp_write_conditionally_and_verify_full_object() {
    for mcp in [false, true] {
        let mut h = Harness::new(SINGLE);
        let state = Arc::new(Mutex::new(SINGLE.to_string()));
        let get = h.stateful_read(state.clone(), 2);
        let put = h.put(state.clone(), 204, 1);
        let result = h.call(mcp, false, false);
        assert_eq!(result["write_outcome"], "verified");
        assert_eq!(result["after"]["end"], "2026-10-27T08:00:00Z");
        let saved = state.lock().unwrap();
        assert!(saved.contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:guest@example.test"));
        assert!(saved.contains("X-CUSTOM:keep"));
        get.assert();
        put.assert();
    }
}

#[test]
fn generated_occurrence_is_expanded_at_matching_etag_and_written_as_exception() {
    for mcp in [false, true] {
        let mut h = Harness::new(SERIES);
        let state = Arc::new(Mutex::new(SERIES.to_string()));
        let get = h.stateful_read(state.clone(), 2);
        let put = h.put(state.clone(), 204, 1);
        let expanded = SINGLE.replace(
            "SUMMARY:Planning",
            "SUMMARY:Weekly\r\nRECURRENCE-ID:20261020T073000Z",
        );
        let report = h
            .server
            .mock("REPORT", "/cal/team/")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex("calendar-multiget".into()),
                Matcher::Regex("<c:expand start=\"20261020T073000Z\"".into()),
            ]))
            .with_status(207)
            .with_body(data_report(&expanded))
            .expect(1)
            .create();
        let result = h.call(mcp, false, true);
        assert_eq!(result["after"]["recurrence_id"], "2026-10-20T07:30:00Z");
        let saved = state.lock().unwrap();
        assert_eq!(saved.matches("BEGIN:VEVENT").count(), 2);
        assert_eq!(saved.matches("RRULE:FREQ=WEEKLY;COUNT=10").count(), 1);
        assert!(saved.contains("RECURRENCE-ID;TZID=Europe/Samara:20261020T113000"));
        get.assert();
        put.assert();
        report.assert();
    }
}

#[test]
fn conflict_is_not_retried_or_reported_as_success() {
    let mut h = Harness::new(SINGLE);
    let get = h.read(SINGLE, 1);
    let put = h
        .server
        .mock("PUT", HREF)
        .match_header("If-Match", "\"v1\"")
        .with_status(412)
        .expect(1)
        .create();
    h.cli(&[])
        .assert()
        .failure()
        .stderr(predicates::str::contains("412"));
    get.assert();
    put.assert();
}

#[test]
fn reviewed_version_mismatch_prevents_write() {
    let mut h = Harness::new(SINGLE);
    let get = h.read(SINGLE, 1);
    let put = h.server.mock("PUT", HREF).expect(0).create();
    h.cli(&["--if-match", "\"stale\""])
        .assert()
        .failure()
        .stderr(predicates::str::contains("changed since review"));
    get.assert();
    put.assert();
}

#[test]
fn uncertain_server_response_is_reconciled_without_second_write() {
    let mut h = Harness::new(SINGLE);
    let state = Arc::new(Mutex::new(SINGLE.to_string()));
    let get = h.stateful_read(state.clone(), 2);
    let put = h.put(state, 500, 1);
    let result = h.call(false, false, false);
    assert_eq!(result["write_outcome"], "verified_after_uncertain_response");
    get.assert();
    put.assert();
}

#[test]
fn successful_put_with_wrong_readback_is_unverified() {
    let mut h = Harness::new(SINGLE);
    let get = h.read(SINGLE, 2);
    let put = h
        .server
        .mock("PUT", HREF)
        .with_status(204)
        .expect(1)
        .create();
    h.cli(&[])
        .assert()
        .failure()
        .stderr(predicates::str::contains("outcome is unverified"));
    get.assert();
    put.assert();
}

#[test]
fn dropped_put_response_is_reconciled_with_one_write_attempt() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let h = Harness::new(SINGLE);
    let config_path = h.config.path().join("accounts.toml");
    let config = fs::read_to_string(&config_path)
        .unwrap()
        .replace(&h.server.url(), &url);
    fs::write(config_path, config).unwrap();
    let worker = std::thread::spawn(move || {
        let mut stored = SINGLE.to_string();
        let mut writes = 0;
        let mut requests = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && requests < 7 {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let headers = String::from_utf8(bytes).unwrap();
            let first = headers.lines().next().unwrap().to_string();
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            requests += 1;
            if first.starts_with("PUT ") {
                stored = String::from_utf8(body).unwrap();
                writes += 1;
                // The server commits and closes the socket without any HTTP response.
                continue;
            }
            let (status, body) = if first.starts_with("GET ") {
                (200, stored.clone())
            } else if first.starts_with("REPORT ") {
                (207, data_report(SINGLE))
            } else if first.starts_with("PROPFIND /principal/ ") {
                (
                    207,
                    multistatus(
                        "<c:calendar-home-set><d:href>/cal/</d:href></c:calendar-home-set>",
                        "/principal/",
                    ),
                )
            } else if first.starts_with("PROPFIND /cal/ ") {
                (
                    207,
                    multistatus(
                        "<d:displayname>Team</d:displayname><d:resourcetype><d:collection/><c:calendar/></d:resourcetype>",
                        "/cal/team/",
                    ),
                )
            } else {
                (
                    207,
                    multistatus(
                        "<d:current-user-principal><d:href>/principal/</d:href></d:current-user-principal>",
                        "/",
                    ),
                )
            };
            write!(stream,"HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        writes
    });
    let output = h
        .cli(&[])
        .timeout(Duration::from_secs(8))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["write_outcome"], "verified_after_uncertain_response");
    assert_eq!(worker.join().unwrap(), 1);
}

#[test]
fn readback_that_loses_an_attendee_is_unverified() {
    let mut h = Harness::new(SINGLE);
    let state = Arc::new(Mutex::new(SINGLE.to_string()));
    let get = h.stateful_read(state.clone(), 2);
    let put = h
        .server
        .mock("PUT", HREF)
        .with_status(204)
        .with_body_from_request(move |req| {
            *state.lock().unwrap() = req
                .utf8_lossy_body()
                .unwrap()
                .lines()
                .filter(|l| !l.starts_with("ATTENDEE"))
                .collect::<Vec<_>>()
                .join("\r\n")
                + "\r\n";
            Vec::new()
        })
        .expect(1)
        .create();
    h.cli(&[])
        .assert()
        .failure()
        .stderr(predicates::str::contains("outcome is unverified"));
    get.assert();
    put.assert();
}

#[test]
fn mcp_rejects_mistyped_dry_run_before_accessing_an_account() {
    let h = Harness::new(SINGLE);
    let input = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"yacli.calendar.update","arguments":{"account":"missing","calendar":"team","uid":"series","scope":"event","start":"2026-10-27T07:30:00Z","dry_run":"true"}}}),
    ].iter().map(|v| v.to_string()+"\n").collect::<String>();
    let output = h
        .command()
        .arg("mcp")
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let response: Value = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["id"] == 2)
        .unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("dry_run")
    );
}

#[test]
fn initial_occurrence_without_server_recurrence_id_can_be_moved() {
    for mcp in [false, true] {
        let mut h = Harness::new(&SERIES.replace("20261013T", "20261020T"));
        let state = Arc::new(Mutex::new(SERIES.replace("20261013T", "20261020T")));
        let get = h.stateful_read(state.clone(), 2);
        let put = h.put(state.clone(), 204, 1);
        let expanded = SINGLE.replace("SUMMARY:Planning", "SUMMARY:Weekly");
        let report = h
            .server
            .mock("REPORT", "/cal/team/")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex("calendar-multiget".into()),
                Matcher::Regex("<c:expand start=\"20261020T073000Z\"".into()),
            ]))
            .with_status(207)
            .with_body(data_report(&expanded))
            .expect(1)
            .create();
        let result = h.call(mcp, false, true);
        assert_eq!(result["after"]["recurrence_id"], "2026-10-20T07:30:00Z");
        let saved = state.lock().unwrap();
        assert_eq!(saved.matches("BEGIN:VEVENT").count(), 2);
        assert_eq!(saved.matches("RRULE:FREQ=WEEKLY;COUNT=10").count(), 1);
        assert!(saved.contains("RECURRENCE-ID;TZID=Europe/Samara:20261020T113000"));
        get.assert();
        put.assert();
        report.assert();
    }
}

#[test]
fn weak_etag_prevents_a_write() {
    let mut h = Harness::new(SINGLE);
    let get = h
        .server
        .mock("GET", HREF)
        .with_status(200)
        .with_header("ETag", "W/\"weak\"")
        .with_body(SINGLE)
        .expect(1)
        .create();
    let put = h.server.mock("PUT", HREF).expect(0).create();
    h.cli(&[])
        .assert()
        .failure()
        .stderr(predicates::str::contains("strong ETag"));
    get.assert();
    put.assert();
}

#[test]
fn resource_outside_selected_calendar_prevents_get_and_put() {
    let mut h = Harness::new(SINGLE);
    h._discovery.pop().unwrap().remove();
    let lookup = h
        .server
        .mock("REPORT", "/cal/team/")
        .match_body(Matcher::Regex("prop-filter name=\"UID\"".into()))
        .with_status(207)
        .with_body(data_report(SINGLE).replace(HREF, "/cal/other/item.ics"))
        .expect(1)
        .create();
    let get = h.server.mock("GET", Matcher::Any).expect(0).create();
    let put = h.server.mock("PUT", Matcher::Any).expect(0).create();
    h.cli(&[])
        .assert()
        .failure()
        .stderr(predicates::str::contains("outside the selected calendar"));
    lookup.assert();
    get.assert();
    put.assert();
}

#[test]
fn changed_etag_during_expansion_prevents_a_write() {
    let mut h = Harness::new(SERIES);
    let get = h.read(SERIES, 1);
    let expanded = SINGLE.replace(
        "SUMMARY:Planning",
        "SUMMARY:Weekly\r\nRECURRENCE-ID:20261020T073000Z",
    );
    let report = h
        .server
        .mock("REPORT", "/cal/team/")
        .match_body(Matcher::Regex("calendar-multiget".into()))
        .with_status(207)
        .with_body(data_report(&expanded).replace("\"v1\"", "\"v2\""))
        .expect(1)
        .create();
    let put = h.server.mock("PUT", HREF).expect(0).create();
    h.command()
        .args([
            "calendar",
            "move",
            "series",
            "--account",
            "mock",
            "--calendar",
            "team",
            "--scope",
            "occurrence",
            "--occurrence",
            "2026-10-20T07:30:00Z",
            "--start",
            "2026-10-27T07:30:00Z",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "changed during occurrence expansion",
        ));
    get.assert();
    report.assert();
    put.assert();
}

#[test]
fn unquoted_numeric_etag_is_quoted_for_conditional_put() {
    let mut h = Harness::new(SINGLE);
    let state = Arc::new(Mutex::new(SINGLE.to_string()));
    let read_state = state.clone();
    let get = h
        .server
        .mock("GET", HREF)
        .with_status(200)
        .with_header("ETag", "1700000000000")
        .with_body_from_request(move |_| read_state.lock().unwrap().as_bytes().to_vec())
        .expect(2)
        .create();
    let put = h
        .server
        .mock("PUT", HREF)
        .match_header("If-Match", "\"1700000000000\"")
        .with_status(204)
        .with_body_from_request(move |req| {
            *state.lock().unwrap() = req.utf8_lossy_body().unwrap().into_owned();
            Vec::new()
        })
        .expect(1)
        .create();
    let output = h
        .cli(&["--if-match", "1700000000000"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["write_outcome"], "verified");
    assert_eq!(result["before"]["etag"], "\"1700000000000\"");
    get.assert();
    put.assert();
}
