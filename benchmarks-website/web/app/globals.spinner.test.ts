// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright the Vortex contributors

// @vitest-environment node

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

const css = readFileSync(join(__dirname, 'globals.css'), 'utf8');

describe('PR-5.0.95 spinner CSS', () => {
  it('defines a spin keyframes animation and a spinner rule', () => {
    expect(css).toMatch(/@keyframes\s+chart-spin/);
    expect(css).toMatch(/\.chart-spinner\b/);
  });

  it('disables the spinner animation under prefers-reduced-motion: reduce', () => {
    const reduced = css.match(
      /@media\s*\(prefers-reduced-motion:\s*reduce\)\s*\{[\s\S]*?\}/g,
    );
    expect(reduced).not.toBeNull();
    expect(reduced!.join('\n')).toMatch(/\.chart-spinner[\s\S]*animation:\s*none/);
  });
});
