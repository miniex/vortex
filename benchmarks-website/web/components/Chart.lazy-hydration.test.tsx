// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright the Vortex contributors

// @vitest-environment jsdom

import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { Chart } from '@/components/Chart';
import { hydrationQueue } from '@/lib/chart-store';

vi.mock('@/lib/chart-js', () => ({
  loadChartJs: () => new Promise(() => {}),
}));

// jsdom has no IntersectionObserver. This mock records each instance and its
// observed elements, and exposes `fire()` to simulate a card scrolling into
// view. One instance is created per chart card (the mount effect arms one IO
// per card), in card-registration (DOM/visual) order.
class MockIO {
  static instances: MockIO[] = [];
  callback: IntersectionObserverCallback;
  elements: Element[] = [];
  disconnected = false;
  constructor(cb: IntersectionObserverCallback) {
    this.callback = cb;
    MockIO.instances.push(this);
  }
  observe(el: Element): void {
    this.elements.push(el);
  }
  unobserve(el: Element): void {
    this.elements = this.elements.filter((e) => e !== el);
  }
  disconnect(): void {
    this.disconnected = true;
    this.elements = [];
  }
  takeRecords(): IntersectionObserverEntry[] {
    return [];
  }
  /** Simulate this card scrolling into (or out of) view. */
  fire(isIntersecting = true): void {
    this.callback(
      this.elements.map(
        (el) => ({ target: el, isIntersecting }) as unknown as IntersectionObserverEntry,
      ),
      this as unknown as IntersectionObserver,
    );
  }
}

function windowedPayload(total: number) {
  return {
    display_name: 'q',
    unit_kind: 'time_ns',
    history: {
      total_commits: total,
      start_index: total - 100,
      loaded_commits: 100,
      complete: false,
    },
    commits: Array.from({ length: 100 }, (_, i) => ({
      sha: `sha${i}`,
      timestamp: `2026-01-01T00:00:${String(i).padStart(2, '0')}Z`,
      message: `c${i}`,
      url: `https://github.com/x/y/commit/sha${i}`,
    })),
    series: { 'vortex/nvme': Array.from({ length: 100 }, (_, i) => i + 1) },
    series_meta: { 'vortex/nvme': { engine: 'vortex', format: 'nvme' } },
  };
}

function jsonResponse(body: unknown): Response {
  return { ok: true, status: 200, json: () => Promise.resolve(body) } as unknown as Response;
}

describe('PR-5.0.95 landing-page lazy hydration', () => {
  let container: HTMLElement;
  let root: Root | null = null;
  let fetchCalls: string[];

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    fetchCalls = [];
    MockIO.instances = [];
    vi.stubGlobal('IntersectionObserver', MockIO);
    vi.stubGlobal('fetch', (url: string | URL) => {
      fetchCalls.push(String(url));
      return Promise.resolve(jsonResponse(windowedPayload(3572)));
    });
    container = document.createElement('div');
    document.body.appendChild(container);
  });

  afterEach(async () => {
    await act(async () => {
      root?.unmount();
    });
    root = null;
    container.remove();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  /** Render `n` charts (index 0..n-1) inside one OPEN group disclosure. */
  async function renderGroup(n: number): Promise<void> {
    const mounts = Array.from({ length: n }, (_, i) => `<div id="m${i}"></div>`).join('');
    container.innerHTML =
      '<section class="group-details">' +
      '<details class="group-disclosure" open><summary class="group-summary">g</summary></details>' +
      `<div class="chart-grid">${mounts}</div>` +
      '</section>';
    // Render each island into its OWN root so each gets its own mount effect.
    const roots: Root[] = [];
    await act(async () => {
      for (let i = 0; i < n; i++) {
        const r = createRoot(container.querySelector(`#m${i}`) as HTMLElement);
        roots.push(r);
        r.render(<Chart slug={`s${i}`} name={`q${i}`} index={i} groupSlug="g" />);
      }
    });
    // A teardown handle the shared afterEach can unmount (every island at once).
    root = {
      unmount: () => roots.forEach((r) => r.unmount()),
      render: () => {},
    } as unknown as Root;
    await act(async () => {
      await Promise.resolve();
    });
  }

  function windowFetchCount(): number {
    return fetchCalls.filter((u) => u.includes('/api/chart/') && u.includes('n=100')).length;
  }

  it('opening a group schedules NO fetch until a card intersects', async () => {
    await renderGroup(5);
    expect(MockIO.instances.length).toBe(5);
    expect(windowFetchCount()).toBe(0);
  });

  it('only intersecting (in-viewport) cards hydrate on group open', async () => {
    await renderGroup(5);
    await act(async () => {
      MockIO.instances[0].fire(true);
      MockIO.instances[1].fire(true);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(windowFetchCount()).toBe(2);
  });

  it('an off-viewport card hydrates only once its observer fires', async () => {
    await renderGroup(5);
    expect(windowFetchCount()).toBe(0);
    await act(async () => {
      MockIO.instances[4].fire(true);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(windowFetchCount()).toBe(1);
  });

  it('schedules top cards at a higher priority than lower cards (visual order)', async () => {
    const scheduleSpy = vi.spyOn(hydrationQueue, 'schedule');
    await renderGroup(5);
    await act(async () => {
      MockIO.instances[0].fire(true);
      MockIO.instances[3].fire(true);
      await Promise.resolve();
    });
    const priorities = scheduleSpy.mock.calls.map((c) => c[1]);
    // index 0 => priority 0; index 3 => priority -3; higher drains first.
    expect(priorities).toContain(0);
    expect(priorities).toContain(-3);
    expect(Math.max(...(priorities as number[]))).toBe(0);
  });

  it('does NOT bulk-prefetch every card when the group summary is hovered', async () => {
    await renderGroup(5);
    const summary = container.querySelector('.group-summary') as HTMLElement;
    await act(async () => {
      summary.dispatchEvent(new Event('pointerenter'));
      await Promise.resolve();
    });
    // No summary-hover bulk prefetch: hovering the summary schedules nothing.
    expect(windowFetchCount()).toBe(0);
  });

  it('reopening the group re-arms fresh observers and hydrates previously-unseen cards', async () => {
    await renderGroup(2);
    // Fire card 0's initial observer so it hydrates before close.
    await act(async () => {
      MockIO.instances[0].fire(true);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(windowFetchCount()).toBe(1);
    const instanceCountAfterOpen = MockIO.instances.length;

    const details = container.querySelector('details.group-disclosure') as HTMLDetailsElement;
    // Close the group; this disconnects observers and aborts in-flight fetches.
    await act(async () => {
      details.open = false;
      details.dispatchEvent(new Event('toggle'));
      await Promise.resolve();
    });

    // Reopen: each card's mount effect runs `armHydration` again, creating a
    // fresh `MockIO` instance per card.
    await act(async () => {
      details.open = true;
      details.dispatchEvent(new Event('toggle'));
      await Promise.resolve();
    });
    // Re-arming must have created at least one new `MockIO` instance.
    expect(MockIO.instances.length).toBeGreaterThan(instanceCountAfterOpen);

    // Fire the re-armed observer for card 1 (which was NOT hydrated before close).
    // The newest `MockIO` instances correspond to the re-armed cards; fire the
    // last one to trigger card 1's fetch.
    const fetchCountBeforeRefire = windowFetchCount();
    await act(async () => {
      MockIO.instances[MockIO.instances.length - 1].fire(true);
      await Promise.resolve();
      await Promise.resolve();
    });
    // Card 1 should now have triggered a fetch, proving re-arming created a
    // working observer.
    expect(windowFetchCount()).toBeGreaterThan(fetchCountBeforeRefire);
  });

  it('reopening after a close-while-queued schedules a FRESH fetch (reopen-race regression)', async () => {
    // Never-resolving, signal-honoring fetch stub: the promise only rejects when
    // the signal fires, so the aborted task's rejection has not settled before
    // the synchronous `abortInFlightFetches()` call in the close handler clears
    // the entry refs (UF-1a). This is the critical window the bug lives in.
    const signals: AbortSignal[] = [];
    vi.stubGlobal('fetch', (url: string | URL, init?: { signal?: AbortSignal }) => {
      fetchCalls.push(String(url));
      if (init?.signal) {
        signals.push(init.signal);
      }
      return new Promise<Response>((_res, reject) => {
        init?.signal?.addEventListener('abort', () =>
          reject(init.signal?.reason ?? new DOMException('Aborted', 'AbortError')),
        );
      });
    });

    await renderGroup(2);

    // Fire card 0's observer so its `?n=100` fetch starts and is in-flight.
    await act(async () => {
      MockIO.instances[0].fire(true);
      await Promise.resolve();
    });
    expect(windowFetchCount()).toBe(1);

    const details = container.querySelector('details.group-disclosure') as HTMLDetailsElement;

    // Close the group inside act but do NOT flush extra microtasks afterward: the
    // aborted task's rejection handler has not run yet, so without UF-1a the
    // `initialFetchEntry` ref would still be non-null on reopen.
    await act(async () => {
      details.open = false;
      details.dispatchEvent(new Event('toggle'));
    });

    // Reopen the group. The toggle handler re-arms the IntersectionObserver for
    // each card, creating new MockIO instances.
    await act(async () => {
      details.open = true;
      details.dispatchEvent(new Event('toggle'));
      await Promise.resolve();
    });

    // Fire the re-armed observer for card 0. Without UF-1a this joins the
    // aborting promise (entry ref is stale) and never schedules a new fetch.
    const newInstances = MockIO.instances.slice(2); // instances created on reopen
    const card0Io = newInstances.find((io) => !io.disconnected) ?? newInstances[0];
    await act(async () => {
      card0Io.fire(true);
      await Promise.resolve();
      await Promise.resolve();
    });

    // A second `?n=100` fetch MUST have been issued for card 0. This assertion
    // fails against the pre-UF-1 code (count stays at 1) and passes after.
    expect(windowFetchCount()).toBe(2);
  });

  it('closing the group disconnects observers and aborts in-flight fetches', async () => {
    const signals: AbortSignal[] = [];
    vi.stubGlobal('fetch', (url: string | URL, init?: { signal?: AbortSignal }) => {
      fetchCalls.push(String(url));
      if (init?.signal) {
        signals.push(init.signal);
      }
      return new Promise<Response>((_res, reject) => {
        init?.signal?.addEventListener('abort', () =>
          reject(init.signal?.reason ?? new DOMException('Aborted', 'AbortError')),
        );
      });
    });
    await renderGroup(2);
    await act(async () => {
      MockIO.instances[0].fire(true);
      await Promise.resolve();
    });
    expect(signals.length).toBe(1);
    expect(signals[0].aborted).toBe(false);
    const details = container.querySelector('details.group-disclosure') as HTMLDetailsElement;
    await act(async () => {
      details.open = false;
      details.dispatchEvent(new Event('toggle'));
      await Promise.resolve();
    });
    expect(signals[0].aborted).toBe(true);
    expect(MockIO.instances.every((io) => io.disconnected)).toBe(true);
  });
});
