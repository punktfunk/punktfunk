#!/usr/bin/env python3
"""Tests for scripts/l10n-census.py: the tokenizers, the prose rule, the ratchet.

Run: python3 scripts/l10n-census-test.py
"""
import importlib.util
import os
import subprocess
import tempfile
import unittest

_spec = importlib.util.spec_from_file_location(
    "census", os.path.join(os.path.dirname(os.path.abspath(__file__)), "l10n-census.py"))
census = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(census)

NAMES = census.name_pattern(["Punktfunk", "Steam Deck", "DualSense", "HEVC"])


def texts(src, lang):
    return [t for _, t in census.literals(src, lang, NAMES)]


class Rust(unittest.TestCase):
    def test_comments_and_doc_comments_are_skipped(self):
        src = '// "Not this one"\n/* "Nor /* nested */ this" */\n/// "Nor docs"\nlet a = "Keep this one";'
        self.assertEqual(texts(src, "rust"), ["Keep this one"])

    def test_raw_strings_chars_and_lifetimes(self):
        src = ("fn f<'a>(x: &'a str) -> char { let q = '\"'; let r = r#\"Raw \"quoted\" text\"#;"
               " let e = \"Ends with \\u{2014} a dash\"; '\\'' }")
        self.assertEqual(texts(src, "rust"), ['Raw "quoted" text', "Ends with — a dash"])

    def test_raw_strings_keep_backslashes(self):
        self.assertEqual(texts(r'let p = r"Stored in C:\Users\me";', "rust"), [])

    def test_log_panic_and_error_context_are_skipped(self):
        src = """
            tracing::warn!(error = %e, "Hook did not launch");
            info!("Session started now");
            bail!("Host refused the pin");
            let x = y.context("Open the config file")?;
            let z = w.with_context(|| format!("Read the {path} file"))?;
            let v = o.expect("Value is present here");
            panic!("This never happens");
            eprintln!("Usage: punktfunk now");
            label("Connect to host");
        """
        self.assertEqual(texts(src, "rust"), ["Connect to host"])

    def test_test_code_and_attributes_are_skipped(self):
        src = """
            #[derive(Debug, thiserror::Error)]
            enum E { #[error("Host went away")] Gone }
            #[cfg(test)]
            mod tests { fn t() { assert_eq!(f(), "Inside the tests"); let s = "Test data here"; } }
            #[test]
            fn alone() { let s = "Another test string"; }
            #[cfg(not(test))]
            const SHIPPED: &str = "Shipped without tests";
            const AFTER: &str = "After the tests";
        """
        self.assertEqual(texts(src, "rust"), ["Shipped without tests", "After the tests"])

    def test_inner_cfg_test_skips_the_file(self):
        self.assertEqual(texts('#![cfg(test)]\nconst A: &str = "Never counted here";', "rust"), [])

    def test_line_numbers(self):
        got = census.literals('let a = 1;\n\nlet b = "Third line text";', "rust", NAMES)
        self.assertEqual(got, [(3, "Third line text")])


class Swift(unittest.TestCase):
    def test_interpolation_with_nested_strings(self):
        src = 'Text("Paired with \\(host.name ?? "Unknown host") just now")'
        self.assertEqual(texts(src, "swift"), ["Paired with {} just now"])

    def test_multiline_and_raw(self):
        src = 'let a = """\n    Two line\n    message here\n    """\nlet b = #"Raw "quoted" words \\#(x)"#'
        self.assertEqual(texts(src, "swift"), ["Two line message here", 'Raw "quoted" words {}'])

    def test_loggers_print_and_previews_are_skipped(self):
        src = """
            log.info("Stream started fine")
            Self.sessionLog.debug("Frame came late")
            print("Debug output here")
            fatalError("Should not happen")
            #Preview("Host card") { HostCard(name: "Living Room PC") }
            catalog.add("Keep this label")
        """
        self.assertEqual(texts(src, "swift"), ["Keep this label"])


class Kotlin(unittest.TestCase):
    def test_templates_and_chars(self):
        src = 'val q = \'"\'\nval s = "$count hosts are online"\nval t = "${host.name("x")} is offline"'
        self.assertEqual(texts(src, "kotlin"), ["{} hosts are online", "{} is offline"])

    def test_raw_strings(self):
        self.assertEqual(texts('val s = """Raw "text" with $name inside"""', "kotlin"),
                         ['Raw "text" with {} inside'])

    def test_log_and_preconditions_are_skipped(self):
        src = """
            Log.i(TAG, "Session started fine")
            requireNotNull(x) { "Host must be set" }
            error("Never reached here")
            Text("Keep this label")
        """
        self.assertEqual(texts(src, "kotlin"), ["Keep this label"])


class TypeScript(unittest.TestCase):
    def test_templates_regex_and_console(self):
        src = """
            const r = /it's "quoted"/g;
            const a = x / 2;
            console.warn("Plugin failed to load");
            toast(`Paired with ${host.name} now`);
        """
        self.assertEqual(texts(src, "ts"), ["Paired with {} now"])

    def test_jsx_text(self):
        src = """
            const C = () => (
              <div style={{ opacity: 0.8 }}>
                Arm pairing on the host (its console), then enter the PIN.
              </div>
            );
            const n = a > b ? 1 : 2;
            if (x < y && y > z) { go(); }
        """
        self.assertEqual(texts(src, "tsx"),
                         ["Arm pairing on the host (its console), then enter the PIN."])


class Prose(unittest.TestCase):
    def check(self, text):
        return census.is_prose(text, NAMES)

    def test_counts(self):
        for t in ["Statistics size", "Couldn't reach the host — it may be asleep.",
                  "Auto-wake on connect", "{} advanced settings changed", "{name} is offline",
                  "Open Steam Deck settings", "DualSense haptics self-test", "Copy Link"]:
            self.assertTrue(self.check(t), t)

    def test_does_not_count(self):
        for t in ["Settings", "io.unom.punktfunk", "/usr/bin/punktfunk", "--pin", "{} hosts",
                  "Steam Deck", "DualSense", "player_stats_scale_pct", "open config file",
                  "read body: {e}", "{}x{} @ {} fps", "C:\\Program Files\\Punktfunk",
                  "https://docs.punktfunk.unom.io", "width=1920", ".row { color: red; }",
                  "{\"label\": \"Open Punktfunk\"}", "HEVC (H.265)"]:
            self.assertFalse(self.check(t), t)


class Tree(unittest.TestCase):
    def test_ignored_files_do_not_count(self):
        with tempfile.TemporaryDirectory() as repo:
            def put(rel, text=""):
                path = os.path.join(repo, rel)
                os.makedirs(os.path.dirname(path), exist_ok=True)
                with open(path, "w", encoding="utf-8") as f:
                    f.write(text)
            try:
                subprocess.run(["git", "-C", repo, "init", "-q"], check=True)
            except (OSError, subprocess.CalledProcessError):
                self.skipTest("git is not available")
            put(".gitignore", "Generated/\n")
            put("app/Home.swift")
            put("app/Generated/L10n.swift")
            put("app/Tests/HomeTests.swift")
            rows = [os.path.relpath(p, repo) for p in census.surface_files(repo, ["app"])]
            self.assertEqual(rows, ["app/Home.swift"])


class Ratchet(unittest.TestCase):
    def test_new_literals_counts_repeats(self):
        stored = ["a.rs\tOne line", "a.rs\tTwo line"]
        today = ["a.rs\tOne line", "a.rs\tOne line", "b.rs\tNew line"]
        self.assertEqual(census.new_literals(today, stored), ["a.rs\tOne line", "b.rs\tNew line"])
        self.assertEqual(census.new_literals(stored[:1], stored), [])


if __name__ == "__main__":
    unittest.main()
