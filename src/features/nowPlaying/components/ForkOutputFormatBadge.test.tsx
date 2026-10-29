import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, screen, waitFor } from '@testing-library/react';
import { isTauri } from '@tauri-apps/api/core';
import { renderWithProviders } from '@/test/helpers/renderWithProviders';
import { onInvoke, invokeMock, emitTauriEvent, tauriMockListenerCount } from '@/test/mocks/tauri';
import { ForkOutputFormatBadge } from './ForkOutputFormatBadge';

const EVENT = 'fork:audio-output-format';
const fmt = (over: Record<string, unknown> = {}) => ({
  rateHz: 192000, channels: 2, resampledFromHz: null, policy: 'seamless', backend: 'shared', fallbackReason: null, bitPerfect: false, ...over,
});
const status = (over: Record<string, unknown> = {}) => ({
  enabled: true, policy: 'seamless', ceilingHz: 192000, streamRateHz: 192000, mixRateHz: 192000, lastFormat: null,
  backend: 'shared', exclusive: null, bitPerfect: false, lastFallbackReason: null,
  config: { enabled: true, policy: 'seamless', backend: 'shared', ceilingHz: null, resyncSilenceMs: 250, chunkFrames: 1024 },
  ...over,
});

beforeEach(() => {
  vi.mocked(isTauri).mockReturnValue(true);
});

describe('ForkOutputFormatBadge', () => {
  it('renders from the last format in the status', async () => {
    onInvoke('fork_hires_status', () => status({ lastFormat: fmt() }));
    renderWithProviders(<ForkOutputFormatBadge />);
    const badge = await screen.findByText('→ 192 kHz');
    expect(badge).toHaveAttribute('title', 'Fork engine output: shared mode');
  });

  it('updates when a track starts on the engine', async () => {
    onInvoke('fork_hires_status', () => status({ lastFormat: fmt() }));
    renderWithProviders(<ForkOutputFormatBadge />);
    await screen.findByText('→ 192 kHz');

    act(() => emitTauriEvent(EVENT, fmt({ rateHz: 96000 })));
    expect(screen.getByText('→ 96 kHz')).toBeInTheDocument();
  });

  it('shows the native rate when the track was resampled', async () => {
    onInvoke('fork_hires_status', () => status());
    renderWithProviders(<ForkOutputFormatBadge />);
    await waitFor(() => expect(tauriMockListenerCount(EVENT)).toBe(1));

    act(() => emitTauriEvent(EVENT, fmt({ resampledFromHz: 44100 })));
    expect(screen.getByText('→ 192 kHz (from 44.1)')).toBeInTheDocument();
  });

  it('renders nothing when the engine is disabled or no format yet', async () => {
    onInvoke('fork_hires_status', () => status({ enabled: false, lastFormat: fmt() }));
    const { container, unmount } = renderWithProviders(<ForkOutputFormatBadge />);
    await waitFor(() => expect(tauriMockListenerCount(EVENT)).toBe(1));
    expect(container).toBeEmptyDOMElement();
    unmount();

    onInvoke('fork_hires_status', () => status({ lastFormat: null }));
    const second = renderWithProviders(<ForkOutputFormatBadge />);
    await waitFor(() => expect(tauriMockListenerCount(EVENT)).toBe(1));
    expect(second.container).toBeEmptyDOMElement();
  });

  it('removes its listener on unmount', async () => {
    onInvoke('fork_hires_status', () => status());
    const { unmount } = renderWithProviders(<ForkOutputFormatBadge />);
    await waitFor(() => expect(tauriMockListenerCount(EVENT)).toBe(1));
    unmount();
    expect(tauriMockListenerCount(EVENT)).toBe(0);
  });
});

describe('ForkOutputFormatBadge exclusive backend', () => {
  it('shows exclusive and bit-perfect from the event', async () => {
    onInvoke('fork_hires_status', () => status());
    renderWithProviders(<ForkOutputFormatBadge />);
    await waitFor(() => expect(tauriMockListenerCount(EVENT)).toBe(1));

    act(() => emitTauriEvent(EVENT, fmt({ backend: 'exclusive', bitPerfect: true, resampledFromHz: 44100 })));
    const badge = screen.getByText('→ 192 kHz (from 44.1) · exclusive · bit-perfect');
    expect(badge.getAttribute('title')).toMatch(/exclusive mode, bit-perfect/);
  });

  it('shows exclusive without bit-perfect', async () => {
    onInvoke('fork_hires_status', () => status({ lastFormat: fmt({ backend: 'exclusive' }) }));
    renderWithProviders(<ForkOutputFormatBadge />);
    expect(await screen.findByText('→ 192 kHz · exclusive')).toBeInTheDocument();
  });

  it('puts the fallback reason in the tooltip', async () => {
    onInvoke('fork_hires_status', () => status({ lastFormat: fmt({ fallbackReason: 'device busy' }) }));
    renderWithProviders(<ForkOutputFormatBadge />);
    const badge = await screen.findByText('→ 192 kHz');
    expect(badge.getAttribute('title')).toBe('Fork engine output: shared mode. Exclusive unavailable: device busy');
  });

  describe('polling', () => {
    beforeEach(() => { vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] }); });
    afterEach(() => { vi.useRealTimers(); });
    const calls = () => invokeMock.mock.calls.filter(c => c[0] === 'fork_hires_status').length;

    it('picks up bitPerfect from the poll and stops polling on unmount', async () => {
      let bp = false;
      onInvoke('fork_hires_status', () => status({
        bitPerfect: bp, lastFormat: fmt({ backend: 'exclusive' }), backend: 'exclusive',
      }));
      const { unmount } = renderWithProviders(<ForkOutputFormatBadge />);
      await act(async () => {});
      expect(screen.getByText('→ 192 kHz · exclusive')).toBeInTheDocument();

      bp = true;
      await act(async () => { await vi.advanceTimersByTimeAsync(2000); });
      expect(screen.getByText('→ 192 kHz · exclusive · bit-perfect')).toBeInTheDocument();

      const before = calls();
      unmount();
      expect(tauriMockListenerCount(EVENT)).toBe(0);
      await act(async () => { await vi.advanceTimersByTimeAsync(10000); });
      expect(calls()).toBe(before);
    });
  });
});
