import { expect, test } from 'bun:test';
import { assertUpgradeableConnection } from '../src/android-remote/upgrade-connection';

test('an explicit active LAN connection survives an opportunistic temporary tunnel', () => {
  for (const status of ['starting', 'checking', 'ready', 'error', 'stopped']) {
    expect(() => assertUpgradeableConnection({ connectionChoice: 'local', localNetworkEnabled: true, tunnelMode: 'quick' }, true, status)).not.toThrow();
  }
});

test('remote-only, unselected and non-listening connections cannot silently lose a quick address', () => {
  for (const connectionChoice of ['quick', undefined] as const) {
    expect(() => assertUpgradeableConnection({ connectionChoice, localNetworkEnabled: true, tunnelMode: 'quick' }, true, 'ready')).toThrow('temporary tunnel');
  }
  for (const [enabled, listening] of [[true, false], [false, true], [false, false]]) {
    expect(() => assertUpgradeableConnection({ connectionChoice: 'local', localNetworkEnabled: enabled, tunnelMode: 'quick' }, listening, 'ready')).toThrow('temporary tunnel');
  }
  expect(() => assertUpgradeableConnection({ connectionChoice: 'named', localNetworkEnabled: false, tunnelMode: 'named' }, false, 'ready')).not.toThrow();
});
