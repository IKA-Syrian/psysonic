import { useCallback, useEffect, useState } from 'react';
import { AudioWaveform } from 'lucide-react';
import { isTauri } from '@tauri-apps/api/core';
import { commands } from '@/generated/bindings';
import type { ForkBackend, ForkHiResConfig, ForkHiResStatus } from '@/generated/bindings';
import SettingsSubSection from '@/features/settings/components/SettingsSubSection';
import { SettingsGroup } from '@/features/settings/components/SettingsGroup';
import { SettingsToggle } from '@/features/settings/components/SettingsToggle';
import { SettingsSubCard, SettingsField } from '@/features/settings/components/SettingsSubCard';
import { loadForkHiResStatus, useForkHiResEnabled } from '@/features/settings/components/forkHiResEngineStore';

/**
 * Fork patch: settings for the fork's hi-res mix engine. Rust is the source of
 * truth (fork-hires.json, loaded at startup); nothing is persisted on the TS
 * side. English only, like the rest of the fork.
 */

const DEFAULT_CEILING_HZ = 192_000;
const CEILINGS: { label: string; hz: number | null }[] = [
  { label: '96 kHz', hz: 96_000 },
  { label: '192 kHz (default)', hz: null },
  { label: '384 kHz', hz: 384_000 },
];

const BACKENDS: { label: string; value: ForkBackend }[] = [
  { label: 'Shared (Windows mixer)', value: 'shared' },
  { label: 'Exclusive (bit-perfect, takes over the device)', value: 'exclusive' },
];
const STATUS_POLL_MS = 1000;

const DEFAULT_CONFIG: ForkHiResConfig = {
  enabled: false, policy: 'seamless', backend: 'shared', ceilingHz: null, resyncSilenceMs: 250, chunkFrames: 1024,
};

export function ForkHiResEngineSection() {
  const tauri = isTauri();
  const [status, setStatus] = useState<ForkHiResStatus | null>(null);
  const [config, setConfig] = useState<ForkHiResConfig>(DEFAULT_CONFIG);

  const fill = useCallback((s: ForkHiResStatus, fillControls = true) => {
    setStatus(s);
    if (fillControls) {
      // The saved config, not the effective values: round-trips backend, ceiling and the hidden fields.
      setConfig({ ...DEFAULT_CONFIG, ...s.config, backend: s.config.backend ?? 'shared' });
    }
  }, []);
  const refresh = useCallback(async (fillControls: boolean) => {
    const s = await loadForkHiResStatus();
    if (s) fill(s, fillControls);
  }, [fill]);

  useEffect(() => { void loadForkHiResStatus().then(s => { if (s) fill(s); }); }, [fill]);

  // Live status while mounted. Never touches the controls, only the readout.
  useEffect(() => {
    if (!tauri) return;
    const id = setInterval(() => {
      void commands.forkHiresStatus().then(s => { if (s) setStatus(s); }).catch(() => {});
    }, STATUS_POLL_MS);
    return () => clearInterval(id);
  }, [tauri]);

  const apply = (patch: Partial<ForkHiResConfig>) => {
    // Always start from the saved config so fields without a control are preserved.
    const next: ForkHiResConfig = { ...config, ...patch };
    setConfig(next);
    useForkHiResEnabled.getState().setEnabled(next.enabled);
    // On failure, put the controls back to what Rust actually has.
    void commands.forkHiresSetConfig(next)
      .then(r => refresh(r.status === 'error'))
      .catch(() => refresh(true));
  };

  const effectiveCeiling = config.ceilingHz ?? DEFAULT_CEILING_HZ;
  const ceilingValue = config.ceilingHz === DEFAULT_CEILING_HZ ? null : config.ceilingHz;
  const showWarning = tauri && config.enabled && status !== null && status.backend === 'shared' && status.mixRateHz < effectiveCeiling;
  const ex = status?.exclusive ?? null;

  const segmented = <T,>(options: { label: string; value: T }[], current: T, onPick: (v: T) => void) => (
    <div className="settings-segmented">
      {options.map(o => (
        <button
          key={o.label}
          type="button"
          className={`btn ${o.value === current ? 'btn-primary' : 'btn-ghost'}`}
          disabled={!tauri}
          onClick={() => onPick(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );

  return (
    <SettingsSubSection
      title="Hi-res mix engine (fork)"
      icon={<AudioWaveform size={16} />}
      searchText="hi-res mix engine fork resampler output rate seamless adaptive ceiling crossfade gapless"
    >
      <div className="settings-card">
        <SettingsGroup>
          <SettingsToggle
            label="Hi-res mix engine"
            desc="Plays every track through the high-quality resampler at one output rate, so crossfade and gapless never drop to 44.1 kHz."
            ariaLabel="Enable hi-res mix engine"
            checked={config.enabled}
            disabled={!tauri}
            onChange={enabled => apply({ enabled })}
          />
          <SettingsSubCard>
            <SettingsField
              label="Policy"
              desc="Seamless (recommended): no gaps, even for tracks added late. Adaptive: follows the queue and is bit-perfect for same-rate queues, but may briefly pause when a higher-rate track is added late."
            >
              {segmented(
                [{ label: 'Seamless', value: 'seamless' as const }, { label: 'Adaptive', value: 'adaptive' as const }],
                config.policy,
                policy => apply({ policy }),
              )}
            </SettingsField>
            <SettingsField label="Output">
              {segmented(BACKENDS, config.backend, backend => apply({ backend }))}
              {config.backend === 'exclusive' && (
                <div className="settings-hint" role="note">
                  While Psysonic plays, other apps can't use this device, and the Windows volume slider has no effect. Bit-perfect needs 100 % volume with EQ, normalization and speed changes off. Applies from the next track.
                </div>
              )}
            </SettingsField>
            <SettingsField label="Ceiling" desc="The highest output rate the engine will open.">
              {segmented(
                CEILINGS.map(c => ({ label: c.label, value: c.hz })),
                ceilingValue,
                ceilingHz => apply({ ceilingHz }),
              )}
            </SettingsField>
            <SettingsField
              note={`Output: ${status?.streamRateHz ?? '—'} Hz · Windows mix format: ${status ? status.mixRateHz : '—'} Hz`}
            />
            {tauri && status && (
              <div className="settings-hint" data-testid="fork-hires-live">
                <div>{`In use: ${status.backend === 'exclusive' ? 'Exclusive' : 'Shared'}`}</div>
                {status.backend === 'exclusive' && ex && (
                  <>
                    <div>{`Format: ${ex.format} · ${ex.validBits}-bit · ${ex.rateHz} Hz`}</div>
                    <div>{`Bit-perfect: ${status.bitPerfect ? 'yes' : 'no'}`}</div>
                    <div style={{ opacity: 0.7, fontSize: '0.85em' }}>
                      {`periods ${ex.periods} · late wakes ${ex.lateWakes} · max gap ${ex.maxGapUs / 1000} ms · timeouts ${ex.timeouts}${ex.bitPerfectCheck != null ? ` · check: ${ex.bitPerfectCheck}` : ''}`}
                    </div>
                  </>
                )}
              </div>
            )}
            {status?.lastFallbackReason && (
              <div className="settings-hint settings-hint-info" role="note">
                {`Exclusive mode unavailable on this device: ${status.lastFallbackReason}. Using shared mode.`}
              </div>
            )}
            {!status?.lastFallbackReason && config.backend === 'exclusive' && status?.backend === 'shared' && (
              <div className="settings-hint" role="note">Exclusive selected; applies from the next track</div>
            )}
            {showWarning && status && (
              <div className="settings-hint settings-hint-info" role="note">
                {`Windows is mixing at ${status.mixRateHz / 1000} kHz, so higher-rate tracks are resampled down to it. Set Sound settings → your device → Advanced → Default Format to 24-bit, 192000 Hz, or switch Output to Exclusive.`}
              </div>
            )}
          </SettingsSubCard>
        </SettingsGroup>
      </div>
    </SettingsSubSection>
  );
}
