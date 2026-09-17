//! Web-entry cells: `vs prompt-form` + the loopback browser form.
//!
//! The browser is simulated with a raw `TcpStream` HTTP/1.1 client —
//! portable across the three CI platforms, and the surface is
//! deliberately minimal enough that this is honest coverage.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;

use crate::helpers::{eval_js, open_fixture};
use crate::support::{assert_ok, body_rest, each_available_backend, token_of, TestContext};

/// One HTTP/1.1 request against the entry surface.
/// Returns `(status_line, body)`.
fn http(url: &str, method: &str, form_body: Option<&str>) -> (String, String) {
    let rest = url.strip_prefix("http://").expect("http url");
    let (host, path) = rest.split_once('/').expect("url path");
    let mut stream = TcpStream::connect(host).expect("connect entry surface");
    let payload = form_body.unwrap_or("");
    let req = format!(
        "{method} /{path} HTTP/1.1\r\nHost: {host}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    let status = out.lines().next().unwrap_or("").to_string();
    let body = out
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn body_kv(body: &str, key: &str) -> String {
    body.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('\t')))
        .unwrap_or_else(|| panic!("no {key} line in body:\n{body}"))
        .trim()
        .to_string()
}

/// Full flow: enqueue a two-field form, fetch the browser page,
/// submit values over HTTP, park in prompt-form-wait, observe both
/// fields filled in the real page. Also proves the nonce is
/// single-use and unknown nonces get 410.
#[test]
fn cell_prompt_form_browser_flow() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        // form.html's inputs render as unlabeled `tf` nodes; DOM
        // order is email then password.
        let tf_refs: Vec<u32> = body
            .lines()
            .filter_map(|l| {
                let mut it = l.trim_start().splitn(3, ' ');
                let n = it.next()?.parse::<u32>().ok()?;
                (it.next()? == "tf").then_some(n)
            })
            .collect();
        let (n_email, n_password) = (tf_refs[0], tf_refs[1]);

        // Enqueue without parking; harvest form id + entry URL.
        let r = ctx.vs(&[
            "prompt-form",
            &page,
            &format!("--field={n_email}=Work email"),
            &format!("--field={n_password}=Password,secret"),
            &format!("--token={token}"),
            "--no-wait",
        ]);
        assert_ok("prompt-form enqueue", &r);
        let enqueue_body = body_rest(&r);
        let form_id = body_kv(&enqueue_body, "form");
        let url = body_kv(&enqueue_body, "url");
        assert!(
            url.starts_with("http://127.0.0.1:"),
            "loopback url, got {url}"
        );

        // The browser page renders both fields; the secret one is a
        // password input; labels are shown.
        let (status, page_html) = http(&url, "GET", None);
        assert!(status.contains("200"), "GET form: {status}");
        // Reading the form never burns the link. The URL is relayed
        // as text to a human, and whatever sits on that path — a link
        // preview, a prefetcher, the human reloading — must not
        // destroy the entry they were about to use.
        for probe in [
            url.clone(),
            format!("{url}?utm_source=chat"),
            format!("{url}/"),
        ] {
            let (status, _) = http(&probe, "GET", None);
            assert!(
                status.contains("200"),
                "repeat/decorated GET must still serve the form: {probe} -> {status}"
            );
        }
        assert!(page_html.contains("Work email"), "label 1:\n{page_html}");
        assert!(page_html.contains("Password"), "label 2:\n{page_html}");
        assert!(
            page_html.contains("type=\"password\""),
            "secret field must be masked:\n{page_html}"
        );

        // Entry ids come from the pending list (the same ids a human
        // would fulfill one-by-one at a tty).
        let r = ctx.vs(&["pending", "list"]);
        assert_ok("pending list", &r);
        let pending = body_rest(&r);
        let id_for = |n: u32| {
            pending
                .lines()
                .find(|l| l.split('\t').nth(2) == Some(&n.to_string()))
                .and_then(|l| l.split('\t').next())
                .unwrap_or_else(|| panic!("no pending entry for ref {n}:\n{pending}"))
                .to_string()
        };
        let (id_email, id_password) = (id_for(n_email), id_for(n_password));

        // A waiter that runs out of budget before the human submits
        // must say so, and must leave the form alone. Reporting this
        // as "cancelled, timed out, or unknown" had agents conclude
        // the link was burned and abandon a form still waiting on a
        // human who was simply still typing.
        let r = ctx.vs(&["prompt-form-wait", &form_id, "--timeout-ms=200"]);
        assert!(
            r.stdout.contains("still waiting"),
            "a timed-out wait must read as live, got {:?}",
            r.stdout
        );
        let (status, _) = http(&url, "GET", None);
        assert!(
            status.contains("200"),
            "form must survive the waiter: {status}"
        );

        // Submit both values in one POST, like the browser form does.
        let post = format!("{id_email}=user%40example.com&{id_password}=hunter+2%21");
        let (status, done_html) = http(&url, "POST", Some(&post));
        assert!(status.contains("200"), "POST form: {status}");
        assert!(
            done_html.contains("2 values delivered"),
            "submit page:\n{done_html}"
        );

        // The parked step returns a fresh token once fills ran.
        let r = ctx.vs(&["prompt-form-wait", &form_id, "--timeout-ms=15000"]);
        assert_ok("prompt-form wait", &r);
        let _ = token_of(&r);

        // The values landed in the real inputs, decoded (%40 -> @,
        // + -> space, %21 -> !).
        let email = eval_js(&ctx, &page, "document.getElementById('email').value");
        assert!(
            email.contains("user@example.com"),
            "email filled, got {email:?}"
        );
        let pw = eval_js(&ctx, &page, "document.getElementById('password').value");
        assert!(pw.contains("hunter 2!"), "password filled, got {pw:?}");

        // The nonce was consumed by the POST; replay and guessing die.
        // A spent link says it was already submitted, an unknown one
        // says it is not valid here — the human can tell "you are
        // done" from "that address is wrong".
        let (status, spent_html) = http(&url, "GET", None);
        assert!(status.contains("410"), "used nonce must be gone: {status}");
        assert!(
            spent_html.contains("already submitted"),
            "spent nonce page:\n{spent_html}"
        );
        let base = url.rsplit_once('/').expect("nonce path").0;
        let (status, _) = http(&format!("{base}/nonexistent"), "GET", None);
        assert!(status.contains("410"), "unknown nonce: {status}");
    }
}

/// A fill that fails after the human has typed must not destroy what
/// they typed.
///
/// `wait_form` takes the form out of the queue before the fills run,
/// so a failing fill used to take the values with it: the human's
/// password was gone, and the agent's retry was told the form was
/// unknown. Here the first wait is misaddressed on purpose; the
/// second one, addressed correctly, still lands the value without the
/// human touching anything.
#[test]
fn cell_prompt_form_survives_a_failed_fill() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (session, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email: u32 = body
            .lines()
            .find_map(|l| {
                let mut it = l.trim_start().splitn(3, ' ');
                let n = it.next()?.parse::<u32>().ok()?;
                (it.next()? == "tf").then_some(n)
            })
            .expect("a text field");

        let r = ctx.vs(&[
            "prompt-form",
            &page,
            &format!("--field={n_email}=Work email"),
            &format!("--token={token}"),
            "--no-wait",
        ]);
        assert_ok("prompt-form enqueue", &r);
        let enqueue_body = body_rest(&r);
        let form_id = body_kv(&enqueue_body, "form");
        let url = body_kv(&enqueue_body, "url");

        // The human types and submits.
        let r = ctx.vs(&["pending", "list"]);
        let id = body_rest(&r)
            .lines()
            .next()
            .and_then(|l| l.split('\t').next())
            .expect("one pending entry")
            .to_string();
        let (status, _) = http(&url, "POST", Some(&format!("{id}=user%40example.com")));
        assert!(status.contains("200"), "POST form: {status}");

        // The agent addresses the wrong session. The fill fails.
        let r = ctx.vs(&[
            "prompt-form-wait",
            &form_id,
            "--session=s_definitely_not_a_session",
            "--timeout-ms=5000",
        ]);
        assert!(
            !r.stdout.contains("unknown"),
            "a failed fill must not report the form as unknown: {:?}",
            r.stdout
        );

        // Addressed correctly, the same values still fill the field.
        let r = ctx.vs(&[
            "prompt-form-wait",
            &form_id,
            &format!("--session={session}"),
            "--timeout-ms=15000",
        ]);
        assert_ok("prompt-form wait after a failed fill", &r);
        let email = eval_js(&ctx, &page, "document.getElementById('email').value");
        assert!(
            email.contains("user@example.com"),
            "value survived the failed fill, got {email:?}"
        );
    }
}

/// `vs pending url` mints a URL even with nothing queued, and the
/// page says so instead of erroring.
#[test]
fn cell_pending_url_empty_queue() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, _page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["pending", "url"]);
        assert_ok("pending url", &r);
        let url = body_kv(&body_rest(&r), "url");
        let (status, html) = http(&url, "GET", None);
        assert!(status.contains("200"), "GET: {status}");
        assert!(
            html.contains("No input is currently requested"),
            "empty-queue page:\n{html}"
        );
    }
}

/// Raw HTTP GET that returns the body as bytes (frames are PNG).
fn http_get_bytes(url: &str) -> (String, Vec<u8>) {
    let rest = url.strip_prefix("http://").expect("http url");
    let (host, path) = rest.split_once('/').expect("url path");
    let mut stream = TcpStream::connect(host).expect("connect entry surface");
    stream
        .write_all(
            format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header/body split");
    let status = String::from_utf8_lossy(&out[..out.iter().position(|&b| b == b'\r').unwrap_or(0)])
        .to_string();
    (status, out[split + 4..].to_vec())
}

/// `vs watch`: the viewer page polls `/frame`, frames are real PNGs
/// of the page, no capture files accumulate, and a closed page ends
/// the view with 410.
#[test]
fn cell_watch_live_view() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");

        let r = ctx.vs(&["watch", &page]);
        assert_ok("watch", &r);
        let url = body_kv(&body_rest(&r), "url");
        assert!(url.contains("/live/"), "live url, got {url}");

        let (status, html) = http_get_bytes(&url);
        assert!(status.contains("200"), "viewer page: {status}");
        let html = String::from_utf8_lossy(&html);
        assert!(html.contains("/frame"), "viewer polls frames:\n{html}");

        let frame_url = format!("{url}/frame");
        let (status, png) = http_get_bytes(&frame_url);
        assert!(status.contains("200"), "frame: {status}");
        assert_eq!(&png[..4], b"\x89PNG", "frame must be a PNG");

        // Frames are transient: the captures dir must not accumulate.
        let captures = ctx.home_path().join("captures");
        let count = std::fs::read_dir(&captures).map_or(0, std::iter::Iterator::count);
        assert_eq!(count, 0, "live frames must not persist in {captures:?}");

        // Closing the page ends the view.
        let r = ctx.vs(&["close", &page]);
        assert_ok("close page", &r);
        let (status, _) = http_get_bytes(&frame_url);
        assert!(status.contains("410"), "closed page frame: {status}");
    }
}
