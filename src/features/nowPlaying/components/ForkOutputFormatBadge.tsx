import { useEffect, useState } from 'react';
import { isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { commands } from '@/generated/bindings';
import type { ForkOutputFormat } from '@/generated/bindings';

/** Fork patch: shows the hi-res mix engine's output rate next to the track's own format badges. */
const BIT_PERFECT_POLL_MS = 2000;
const khz = (hz: number) => String(Number((hz / 1000).toFixed(1)));

export function ForkOutputFormatBadge() {
  const [enabled, setEnabled] = useState(false);
  const [format, setFormat] = useState<ForkOutputFormat | null>(null);
  const [bitPerfect, setBitPerfect] = useState(false);

  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void commands.forkHiresStatus().then(s => {
      if (cancelled) return;
      setEnabled(prev => prev || s.enabled);
      // Keep a format an event delivered before this read finished.
      setFormat(prev => prev ?? s.lastFormat);
      setBitPerfect(s.bitPerfect);
    }).catch(() => {});
    void listen<ForkOutputFormat>('fork:audio-output-format', e => {
      setEnabled(true);
      setFormat(e.payload);
      setBitPerfect(e.payload.bitPerfect);
    }).then(fn => {
      if (cancelled) fn(); else unlisten = fn;
    }).catch(() => {});
    // bitPerfect flips during playback without an event, so poll it.
    const id = setInterval(() => {
      void commands.forkHiresStatus().then(s => {
        if (!cancelled && s) setBitPerfect(s.bitPerfect);
      }).catch(() => {});
    }, BIT_PERFECT_POLL_MS);
    return () => { cancelled = true; clearInterval(id); unlisten?.(); };
  }, []);

  if (!enabled || !format) return null;
  const exclusive = format.backend === 'exclusive';
  const tip = [
    exclusive
      ? `Fork engine output: exclusive mode${bitPerfect ? ', bit-perfect' : ', not bit-perfect'}`
      : 'Fork engine output: shared mode',
    format.fallbackReason ? `Exclusive unavailable: ${format.fallbackReason}` : null,
  ].filter(Boolean).join('. ');
  return (
    <span className="np-badge" data-tooltip={tip} title={tip}>
      {`→ ${khz(format.rateHz)} kHz`}
      {format.resampledFromHz != null && ` (from ${khz(format.resampledFromHz)})`}
      {exclusive && ' · exclusive'}
      {exclusive && bitPerfect && ' · bit-perfect'}
    </span>
  );
}
