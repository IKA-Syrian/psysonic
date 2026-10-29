import { useCallback, useEffect, useState } from 'react';
import { AudioWaveform } from 'lucide-react';
import { isTauri } from '@tauri-apps/api/core';
import { commands } from '@/generated/bindings';
import type { ForkHiResConfig, ForkHiResStatus } from '@/generated/bindings';
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

type Draft = { enabled: boolean; policy: ForkHiResConfig['policy']; ceilingHz: number | null };

export function ForkHiResEngineSection() {
  const tauri = isTauri();
  const [status, setStatus] = useState<ForkHiResStatus | null>(null);
  const [config, setConfig] = useState<Draft>({ enabled: false, policy: 'seamless', ceilingHz: null });

  const fill = useCallback((s: ForkHiResStatus, fillControls = true) => {
    setStatus(s);
    if (fillControls) {
      setConfig({ enabled: s.enabled, policy: s.policy, ceilingHz: s.ceilingHz === DEFAULT_CEILING_HZ ? null : s.ceilingHz });
    }
  }, []);
  const refresh = useCallback(async (fillControls: boolean) => {
    const s = await loadForkHiResStatus();
    if (s) fill(s, fillControls);
  }, [fill]);

  useEffect(() => { void loadForkHiResStatus().then(s => { if (s) fill(s); }); }, [fill]);

  const apply = (next: Draft) => {
    setConfig(next);
    useForkHiResEnabled.getState().setEnabled(next.enabled);
    // resyncSilenceMs / chunkFrames have no UI yet; send the defaults.
    const full: ForkHiResConfig = { ...next, resyncSilenceMs: 250, chunkFrames: 1024 };
    // On failure, put the controls back to what Rust actually has.
    void commands.forkHiresSetConfig(full)
      .then(r => refresh(r.status === 'error'))
      .catch(() => refresh(true));
  };

  const effectiveCeiling = config.ceilingHz ?? DEFAULT_CEILING_HZ;
  const showWarning = tauri && config.enabled && status !== null && status.mixRateHz < effectiveCeiling;

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
            onChange={enabled => apply({ ...config, enabled })}
          />
          <SettingsSubCard>
            <SettingsField
              label="Policy"
              desc="Seamless (recommended): no gaps, even for tracks added late. Adaptive: follows the queue and is bit-perfect for same-rate queues, but may briefly pause when a higher-rate track is added late."
            >
              {segmented(
                [{ label: 'Seamless', value: 'seamless' as const }, { label: 'Adaptive', value: 'adaptive' as const }],
                config.policy,
                policy => apply({ ...config, policy }),
              )}
            </SettingsField>
            <SettingsField label="Ceiling" desc="The highest output rate the engine will open.">
              {segmented(
                CEILINGS.map(c => ({ label: c.label, value: c.hz })),
                config.ceilingHz,
                ceilingHz => apply({ ...config, ceilingHz }),
              )}
            </SettingsField>
            <SettingsField
              note={`Output: ${status?.streamRateHz ?? '—'} Hz · Windows mix format: ${status ? status.mixRateHz : '—'} Hz`}
            />
            {showWarning && status && (
              <div className="settings-hint settings-hint-info" role="note">
                {`Windows is mixing at ${status.mixRateHz / 1000} kHz, so higher-rate tracks are resampled down to it. Set Sound settings → your device → Advanced → Default Format to 24-bit, 192000 Hz, or use exclusive mode once it ships.`}
              </div>
            )}
          </SettingsSubCard>
        </SettingsGroup>
      </div>
    </SettingsSubSection>
  );
}
