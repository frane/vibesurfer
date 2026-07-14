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
        let (status, _) = http(&url, "GET", None);
        assert!(status.contains("410"), "used nonce must be gone: {status}");
        let base = url.rsplit_once('/').expect("nonce path").0;
        let (status, _) = http(&format!("{base}/nonexistent"), "GET", None);
        assert!(status.contains("410"), "unknown nonce: {status}");
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
