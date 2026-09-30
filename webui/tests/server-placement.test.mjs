import assert from 'node:assert/strict';
import { test } from 'node:test';
import { nextServerCardPlacement } from '../public-src/js/server-placement.js';

const initial = { placement: 'above', restreaming: null };

test('first idle status moves to below', () => {
  const next = nextServerCardPlacement(initial, { type: 'status', restreaming: false });
  assert.deepEqual(next, { placement: 'below', restreaming: false });
});

test('first 转播中 status stays above', () => {
  const next = nextServerCardPlacement(initial, { type: 'status', restreaming: true });
  assert.deepEqual(next, { placement: 'above', restreaming: true });
});

test('a repeated poll does not change placement', () => {
  const above = nextServerCardPlacement(initial, { type: 'status', restreaming: true });
  const again = nextServerCardPlacement(above, { type: 'status', restreaming: true });
  assert.strictEqual(again, above);

  const below = nextServerCardPlacement(initial, { type: 'status', restreaming: false });
  const idleAgain = nextServerCardPlacement(below, { type: 'status', restreaming: false });
  assert.strictEqual(idleAgain, below);
});

test('转播中 ending moves to below', () => {
  const above = nextServerCardPlacement(initial, { type: 'status', restreaming: true });
  const next = nextServerCardPlacement(above, { type: 'status', restreaming: false });
  assert.deepEqual(next, { placement: 'below', restreaming: false });
});

test('转播中 starting moves to above', () => {
  const below = nextServerCardPlacement(initial, { type: 'status', restreaming: false });
  const next = nextServerCardPlacement(below, { type: 'status', restreaming: true });
  assert.deepEqual(next, { placement: 'above', restreaming: true });
});

test('refresh-failed keeps the previous placement', () => {
  const below = nextServerCardPlacement(initial, { type: 'status', restreaming: false });
  const failed = nextServerCardPlacement(below, { type: 'refresh-failed' });
  assert.strictEqual(failed, below);

  const fromInitial = nextServerCardPlacement(initial, { type: 'refresh-failed' });
  assert.strictEqual(fromInitial, initial);
});
