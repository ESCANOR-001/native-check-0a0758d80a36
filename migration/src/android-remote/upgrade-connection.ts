import type { AndroidRemoteSettings } from './store';

/** Local selection must not inherit the opportunistic quick tunnel's lifetime.
 * An explicitly selected (or ambiguous older) remote connection cannot lose its
 * temporary address during migration. Named credentials have a separate handoff. */
export function assertUpgradeableConnection(
  settings: Pick<AndroidRemoteSettings, 'connectionChoice' | 'localNetworkEnabled' | 'tunnelMode'>,
  listeningOnLocalNetwork: boolean,
  tunnelStatus: string,
): void {
  const local = settings.connectionChoice === 'local'
    && settings.localNetworkEnabled && listeningOnLocalNetwork;
  if (settings.tunnelMode === 'quick' && tunnelStatus !== 'stopped' && !local) {
    throw new Error('A temporary tunnel cannot retain its address during upgrade; the current bridge remains active.');
  }
}
