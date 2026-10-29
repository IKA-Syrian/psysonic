import { useEffect, useState } from 'react';
import { isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { commands } from '@/generated/bindings';
import type { ForkOutputFormat } from '@/generated/bindings';

/** Fork patch: shows the hi-res mix engine's output rate next to the track's own format badges. */
const khz = (hz: number) => String(Number((hz / 1000).toFixed(1)));

export function ForkOutputFormatBadge() {
  const [enabled, setEnabled] = useState(false);
  const [format, setFormat] = useState<ForkOutputFormat | null>(null);

  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void commands.forkHiresStatus().then(s => {
      if (cancelled) return;
      setEnabled(prev => prev || s.enabled);
      // Keep a format an event delivered before this read finished.
      setFormat(prev => prev ?? s.lastFormat);
    }).catch(() => {});
    void listen<ForkOutputFormat>('fork:audio-output-format', e => {
      setEnabled(true);
      setFormat(e.payload);
    }).then(fn => {
      if (cancelled) fn(); else unlisten = fn;
    }).catch(() => {});
    return () => { cancelled = true; unlisten?.(); };
  }, []);

  if (!enabled || !format) return null;
  return (
    <span className="np-badge" data-tooltip="Fork engine output: shared mode" title="Fork engine output: shared mode">
      {`→ ${khz(format.rateHz)} kHz`}
      {format.resampledFromHz != null && ` (from ${khz(format.resampledFromHz)})`}
    </span>
  );
}
