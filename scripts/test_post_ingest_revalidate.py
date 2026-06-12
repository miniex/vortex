# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: Copyright the Vortex contributors

"""Unit tests for post-ingest.py's best-effort site-cache refresh hook.

These are pure-stdlib tests (no Docker, no psycopg): they exercise
`refresh_site_cache` by monkeypatching `urllib.request.urlopen`, asserting the
bearer header is sent and that every failure is swallowed so the hook can never
change the ingest exit code.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parent


def _load_module(filename: str, modname: str):
    path = SCRIPTS_DIR / filename
    spec = importlib.util.spec_from_file_location(modname, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


post_ingest = _load_module("post-ingest.py", "post_ingest")


class _FakeResponse:
    def __init__(self, body: bytes = b"{}"):
        self._body = body

    def read(self) -> bytes:
        return self._body

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def test_refresh_posts_revalidate_with_bearer(monkeypatch):
    calls: list[tuple[str, dict[str, str], bytes | None]] = []

    def fake_urlopen(req, timeout=None):
        calls.append((req.full_url, dict(req.headers), req.data))
        return _FakeResponse(b'{"groups": []}')

    monkeypatch.setattr(post_ingest.urllib.request, "urlopen", fake_urlopen)
    post_ingest.refresh_site_cache("https://example.test/", "tok", 5.0)

    revalidate = [c for c in calls if c[0].endswith("/api/revalidate")]
    assert revalidate, "expected a POST to /api/revalidate"
    # urllib title-cases header keys, so the bearer lives under "Authorization".
    assert revalidate[0][1].get("Authorization") == "Bearer tok"
    # Revalidate must be the first request issued, before any warm GETs.
    assert calls[0][0].endswith("/api/revalidate"), "revalidate must precede warm GETs"


def test_refresh_skips_warm_when_revalidate_fails(monkeypatch):
    """When the revalidate POST fails, no warm GET must be issued.

    Warming after a failed flush would repopulate the Data Cache with stale data.
    The function must still return normally (never raise).
    """
    calls: list[str] = []

    def fake_urlopen(req, timeout=None):
        calls.append(req.full_url)
        if len(calls) == 1:
            raise OSError("revalidate failed")
        return _FakeResponse(b'{"groups": []}')

    monkeypatch.setattr(post_ingest.urllib.request, "urlopen", fake_urlopen)
    result = post_ingest.refresh_site_cache("https://example.test", "tok", 5.0)

    # Must not raise.
    assert result is None
    # Only the one revalidate attempt should have been made; no warm GETs follow.
    assert len(calls) == 1, f"expected 1 call (the failed revalidate), got {len(calls)}: {calls}"
    assert calls[0].endswith("/api/revalidate")


def test_refresh_swallows_all_failures(monkeypatch):
    def boom(req, timeout=None):
        raise OSError("connection refused")

    monkeypatch.setattr(post_ingest.urllib.request, "urlopen", boom)
    # Must not raise: a cache-refresh failure can never fail an ingest.
    assert post_ingest.refresh_site_cache("https://example.test", "tok", 5.0) is None
