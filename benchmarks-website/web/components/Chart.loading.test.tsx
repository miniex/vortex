// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright the Vortex contributors

// @vitest-environment jsdom

import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { Chart } from '@/components/Chart';
import { fullHistoryQueue } from '@/lib/chart-store';

// Mock Chart.js construction to a NEVER-RESOLVING loader: maybeConstruct awaits
// it forever and never reaches `new Chart(...)`, so the fetch-orchestration path
// runs to completion without constructing a chart in jsdom.
vi.mock('@/lib/chart-js', () => ({
  loadChartJs: () => new Promise(() => {}),
}));

// The payloads flow through the fetch stub as `unknown`, so they need not be
// statically typed as `ChartResponse`; they DO need the correct runtime shape
// (`lib/queries.ts`: ChartResponse has `display_name`/`history`; CommitPoint is
// `{ sha, timestamp, message, url }`) so `normalizeChartPayload` and the chip
// read real values.

/** A latest-100 windowed payload for a chart with `total` commits. */
function windowedPayload(total: number) {
  return {
    display_name: 'tpch q1',
    unit_kind: 'time_ns',
    history: { total_commits: total, start_index: total - 100, loaded_commits: 100, complete: false },
    commits: Array.from({ length: 100 }, (_, i) => ({
      sha: `sha${i}`,
      timestamp: `2026-01-01T00:00:${String(i).padStart(2, '0')}Z`,
      message: `commit ${i}`,
      url: `https://github.com/x/y/commit/sha${i}`,
    })),
    series: { 'vortex/nvme': Array.from({ length: 100 }, (_, i) => i + 1) },
    series_meta: { 'vortex/nvme': { engine: 'vortex', format: 'nvme' } },
  };
}

/** A complete payload (born with all its history, fewer than 100 commits). */
function completePayload(total: number) {
  return {
    display_name: 'polarsignals q0',
    unit_kind: 'time_ns',
    history: { total_commits: total, start_index: 0, loaded_commits: total, complete: true },
    commits: Array.from({ length: total }, (_, i) => ({
      sha: `sha${i}`,
      timestamp: `2026-01-01T00:00:${String(i).padStart(2, '0')}Z`,
      message: `commit ${i}`,
      url: `https://github.com/x/y/commit/sha${i}`,
    })),
    series: { 'vortex/nvme': Array.from({ length: total }, (_, i) => i + 1) },
    series_meta: { 'vortex/nvme': { engine: 'vortex', format: 'nvme' } },
  };
}

describe('Chart opt-in full-history loading', () => {
  let container: HTMLElement;
  let root: Root | null = null;
  let fetchCalls: string[];
  // Per-URL-substring responders; default resolves a windowed payload.
  let responders: { match: (url: string) => boolean; respond: (url: string) => Promise<Response> }[];

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    fetchCalls = [];
    responders = [];
    vi.stubGlobal('fetch', (url: string | URL) => {
      const u = String(url);
      fetchCalls.push(u);
      const r = responders.find((x) => x.match(u));
      if (r) {
        return r.respond(u);
      }
      return Promise.resolve(jsonResponse(windowedPayload(3572)));
    });
    container = document.createElement('div');
    document.body.appendChild(container);
  });

  afterEach(async () => {
    await act(async () => {
      root?.unmount();
    });
    container.remove();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  function jsonResponse(body: unknown): Response {
    return { ok: true, status: 200, json: () => Promise.resolve(body) } as unknown as Response;
  }

  /** Render one Chart island inside an OPEN group disclosure and flush the
   * initial `?n=100` fetch. Returns the chip button (or null). */
  async function renderOpenGroup(slug = 'qm.eyJrIjoidHBjaCJ9'): Promise<HTMLButtonElement | null> {
    container.innerHTML =
      '<section class="group-details">' +
      '<details class="group-disclosure" open><summary class="group-summary">g</summary></details>' +
      '<div class="chart-grid"><div id="mount"></div></div>' +
      '</section>';
    const mount = container.querySelector('#mount') as HTMLElement;
    root = createRoot(mount);
    await act(async () => {
      root?.render(<Chart slug={slug} name="tpch q1" index={0} groupSlug="tpch" />);
    });
    // Let the queued initial fetch and its normalization microtasks settle.
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    return container.querySelector('[data-role="window-chip"]');
  }

  it('opening a group issues the windowed fetch but NO full-history warmup', async () => {
    const scheduleSpy = vi.spyOn(fullHistoryQueue, 'schedule');
    await renderOpenGroup();
    const windowFetches = fetchCalls.filter((u) => u.includes('/api/chart/') && u.includes('n=100'));
    const fullFetches = fetchCalls.filter((u) => u.includes('n=all'));
    expect(windowFetches.length).toBeGreaterThanOrEqual(1);
    expect(fullFetches).toHaveLength(0);
    expect(scheduleSpy).not.toHaveBeenCalled();
  });

  it('shows the window chip "latest 100 of 3,572" for a windowed chart', async () => {
    const chip = await renderOpenGroup();
    expect(chip).not.toBeNull();
    expect(chip?.hasAttribute('hidden')).toBe(false);
    expect(chip?.dataset.state).toBe('windowed');
    expect(chip?.textContent).toBe('latest 100 of 3,572');
  });

  it('hides the chip for a chart born with its complete history', async () => {
    responders.push({ match: (u) => u.includes('n=100'), respond: () => Promise.resolve(jsonResponse(completePayload(40))) });
    const chip = await renderOpenGroup();
    expect(chip?.hasAttribute('hidden')).toBe(true);
  });

  it('chip click loads full history at top priority and reaches "all N"', async () => {
    let resolveFull: (r: Response) => void = () => {};
    responders.push({
      match: (u) => u.includes('n=all'),
      respond: () => new Promise<Response>((res) => { resolveFull = res; }),
    });
    const chip = await renderOpenGroup();
    await act(async () => {
      chip?.click();
      await Promise.resolve();
    });
    expect(fetchCalls.some((u) => u.includes('n=all'))).toBe(true);
    expect(chip?.dataset.state).toBe('loading');
    await act(async () => {
      resolveFull(jsonResponse(completePayload(3572)));
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(chip?.dataset.state).toBe('complete');
    expect(chip?.textContent).toBe('all 3,572');
  });

  it('a failed full fetch surfaces a retry affordance', async () => {
    let rejectFull: (e: unknown) => void = () => {};
    responders.push({
      match: (u) => u.includes('n=all'),
      respond: () => new Promise<Response>((_, rej) => { rejectFull = rej; }),
    });
    const chip = await renderOpenGroup();
    await act(async () => {
      chip?.click();
      await Promise.resolve();
    });
    await act(async () => {
      rejectFull(new Error('boom'));
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(chip?.dataset.state).toBe('error');
    expect(chip?.textContent).toBe('retry');
    expect(chip?.disabled).toBe(false);
  });
});
