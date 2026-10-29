import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { isTauri } from '@tauri-apps/api/core';
import { renderWithProviders } from '@/test/helpers/renderWithProviders';
import { onInvoke, invokeMock } from '@/test/mocks/tauri';
import { ForkHiResEngineSection } from './ForkHiResEngineSection';
import { useForkHiResEnabled, loadForkHiResStatus } from './forkHiResEngineStore';

const status = (over: Record<string, unknown> = {}) => {
  const base = {
    enabled: true,
    policy: 'seamless',
    ceilingHz: 192000,
    streamRateHz: 192000,
    mixRateHz: 192000,
    lastFormat: null,
    backend: 'shared',
    exclusive: null,
    bitPerfect: false,
    lastFallbackReason: null,
    ...over,
  };
  // The saved config defaults to what the other fields imply, like a stream that matches the setting.
  return {
    config: {
      enabled: base.enabled,
      policy: base.policy,
      backend: base.backend ?? 'shared',
      ceilingHz: base.ceilingHz === 192000 ? null : base.ceilingHz,
      resyncSilenceMs: 250,
      chunkFrames: 1024,
    },
    ...base,
  };
};

const setConfigCalls = () => invokeMock.mock.calls.filter(c => c[0] === 'fork_hires_set_config');
const lastConfig = () => setConfigCalls().slice(-1)[0]?.[1]?.config;

beforeEach(() => {
  vi.mocked(isTauri).mockReturnValue(true);
  useForkHiResEnabled.setState({ enabled: false });
  onInvoke('fork_hires_set_config', () => null);
});

describe('ForkHiResEngineSection', () => {
  it('fills its controls from fork_hires_status', async () => {
    onInvoke('fork_hires_status', () => status({ policy: 'adaptive', ceilingHz: 96000, streamRateHz: null, mixRateHz: 96000 }));
    renderWithProviders(<ForkHiResEngineSection />);

    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' })).toBeChecked());
    expect(screen.getByRole('button', { name: 'Adaptive' })).toHaveClass('btn-primary');
    expect(screen.getByRole('button', { name: '96 kHz' })).toHaveClass('btn-primary');
    expect(screen.getByText('Output: — Hz · Windows mix format: 96000 Hz')).toBeInTheDocument();
    expect(useForkHiResEnabled.getState().enabled).toBe(true);
  });

  it('sends the full config when toggling enable', async () => {
    // Stateful like Rust: set_config changes what the next status returns.
    let current = status({ enabled: false });
    onInvoke('fork_hires_status', () => current);
    onInvoke('fork_hires_set_config', (args) => {
      const c = (args as { config: { enabled: boolean } }).config;
      current = status({ enabled: c.enabled });
      return null;
    });
    renderWithProviders(<ForkHiResEngineSection />);
    const toggle = screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' });
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('fork_hires_status'));

    await userEvent.click(toggle);

    await waitFor(() => expect(setConfigCalls()).toHaveLength(1));
    expect(lastConfig()).toEqual({
      enabled: true, policy: 'seamless', backend: 'shared', ceilingHz: null, resyncSilenceMs: 250, chunkFrames: 1024,
    });
    expect(useForkHiResEnabled.getState().enabled).toBe(true);
  });

  it('sends the chosen policy and ceiling (null for 192 kHz)', async () => {
    onInvoke('fork_hires_status', () => status());
    renderWithProviders(<ForkHiResEngineSection />);
    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' })).toBeChecked());

    await userEvent.click(screen.getByRole('button', { name: 'Adaptive' }));
    await waitFor(() => expect(lastConfig()).toMatchObject({ policy: 'adaptive', ceilingHz: null }));

    await userEvent.click(screen.getByRole('button', { name: '384 kHz' }));
    await waitFor(() => expect(lastConfig()).toMatchObject({ policy: 'adaptive', ceilingHz: 384000 }));

    await userEvent.click(screen.getByRole('button', { name: '192 kHz (default)' }));
    await waitFor(() => expect(lastConfig()).toMatchObject({ ceilingHz: null }));
  });

  it('warns only when enabled and the mix rate is below the ceiling', async () => {
    const warning = /Windows is mixing at 48 kHz/;
    onInvoke('fork_hires_status', () => status({ mixRateHz: 48000 }));
    renderWithProviders(<ForkHiResEngineSection />);
    expect(await screen.findByText(warning)).toBeInTheDocument();

    // A 96 kHz ceiling is still above the 48 kHz mix rate, so it still warns.
    await userEvent.click(screen.getByRole('button', { name: '96 kHz' }));
    expect(screen.getByText(warning)).toBeInTheDocument();
  });

  it('does not warn when the mix rate reaches the ceiling or the engine is off', async () => {
    onInvoke('fork_hires_status', () => status({ mixRateHz: 192000 }));
    const first = renderWithProviders(<ForkHiResEngineSection />);
    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' })).toBeChecked());
    expect(screen.queryByText(/Windows is mixing at/)).toBeNull();
    first.unmount();

    onInvoke('fork_hires_status', () => status({ enabled: false, mixRateHz: 48000 }));
    renderWithProviders(<ForkHiResEngineSection />);
    await waitFor(() => expect(screen.getByText(/Windows mix format: 48000 Hz/)).toBeInTheDocument());
    expect(screen.queryByText(/Windows is mixing at/)).toBeNull();
  });

  it('disables the controls outside Tauri', () => {
    vi.mocked(isTauri).mockReturnValue(false);
    renderWithProviders(<ForkHiResEngineSection />);
    expect(screen.getByRole('button', { name: 'Adaptive' })).toBeDisabled();
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

const EXCL = /^Exclusive \(bit-perfect/;
const exclusiveStatus = (over: Record<string, unknown> = {}) => status({
  backend: 'exclusive',
  bitPerfect: true,
  exclusive: { format: 'S24', validBits: 24, rateHz: 96000, periods: 120, lateWakes: 2, maxGapUs: 3500, timeouts: 1, lastError: null, bitPerfectCheck: null },
  ...over,
});
const statusCalls = () => invokeMock.mock.calls.filter(c => c[0] === 'fork_hires_status').length;

describe('ForkHiResEngineSection exclusive backend', () => {
  it('reads backend from status and sends it with the full config', async () => {
    onInvoke('fork_hires_status', () => exclusiveStatus());
    renderWithProviders(<ForkHiResEngineSection />);
    const excl = await screen.findByRole('button', { name: EXCL });
    await waitFor(() => expect(excl).toHaveClass('btn-primary'));

    await userEvent.click(screen.getByRole('button', { name: 'Shared (Windows mixer)' }));
    await waitFor(() => expect(lastConfig()).toEqual({
      enabled: true, policy: 'seamless', backend: 'shared', ceilingHz: null, resyncSilenceMs: 250, chunkFrames: 1024,
    }));
    await userEvent.click(screen.getByRole('button', { name: 'Adaptive' }));
    await waitFor(() => expect(lastConfig()).toMatchObject({ policy: 'adaptive', backend: 'shared' }));
  });

  it('keeps the saved exclusive config and hidden fields when no stream is open', async () => {
    const saved = { enabled: true, policy: 'seamless', backend: 'exclusive', ceilingHz: null, resyncSilenceMs: 400, chunkFrames: 2048 };
    onInvoke('fork_hires_status', () => status({ backend: 'shared', streamRateHz: null, config: saved }));
    renderWithProviders(<ForkHiResEngineSection />);
    const excl = await screen.findByRole('button', { name: EXCL });
    await waitFor(() => expect(excl).toHaveClass('btn-primary'));
    expect(screen.getByRole('button', { name: 'Shared (Windows mixer)' })).not.toHaveClass('btn-primary');
    expect(screen.getByText('Exclusive selected; applies from the next track')).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Adaptive' }));
    await waitFor(() => expect(lastConfig()).toEqual({ ...saved, policy: 'adaptive' }));
  });

  it('treats a status without a backend as shared', async () => {
    onInvoke('fork_hires_status', () => status({ backend: undefined }));
    renderWithProviders(<ForkHiResEngineSection />);
    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' })).toBeChecked());
    expect(screen.getByRole('button', { name: 'Shared (Windows mixer)' })).toHaveClass('btn-primary');
  });

  it('shows the exclusive notice only for exclusive', async () => {
    onInvoke('fork_hires_status', () => status());
    renderWithProviders(<ForkHiResEngineSection />);
    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' })).toBeChecked());
    const notice = /other apps can't use this device/;
    expect(screen.queryByText(notice)).toBeNull();

    await userEvent.click(screen.getByRole('button', { name: EXCL }));
    expect(screen.getByText(notice)).toBeInTheDocument();
    expect(lastConfig()).toMatchObject({ backend: 'exclusive' });
  });

  it('renders the live exclusive status', async () => {
    onInvoke('fork_hires_status', () => exclusiveStatus());
    renderWithProviders(<ForkHiResEngineSection />);
    expect(await screen.findByText('In use: Exclusive')).toBeInTheDocument();
    expect(screen.getByText('Format: S24 · 24-bit · 96000 Hz')).toBeInTheDocument();
    expect(screen.getByText('Bit-perfect: yes')).toBeInTheDocument();
    expect(screen.getByText('periods 120 · late wakes 2 · max gap 3.5 ms · timeouts 1')).toBeInTheDocument();
  });

  it('appends the bit-perfect check result to the diagnostics line when present', async () => {
    onInvoke('fork_hires_status', () => exclusiveStatus({
      exclusive: { format: 'S24', validBits: 24, rateHz: 96000, periods: 120, lateWakes: 2, maxGapUs: 3500, timeouts: 1, lastError: null, bitPerfectCheck: 'match (48000 frames)' },
    }));
    renderWithProviders(<ForkHiResEngineSection />);
    expect(await screen.findByText('periods 120 · late wakes 2 · max gap 3.5 ms · timeouts 1 · check: match (48000 frames)')).toBeInTheDocument();
  });

  it('shows bit-perfect as no when the gate is off', async () => {
    onInvoke('fork_hires_status', () => exclusiveStatus({ bitPerfect: false }));
    renderWithProviders(<ForkHiResEngineSection />);
    expect(await screen.findByText('Bit-perfect: no')).toBeInTheDocument();
  });

  it('shows the fallback message when a reason is set', async () => {
    onInvoke('fork_hires_status', () => status({ lastFallbackReason: 'device busy' }));
    renderWithProviders(<ForkHiResEngineSection />);
    expect(await screen.findByText('Exclusive mode unavailable on this device: device busy. Using shared mode.')).toBeInTheDocument();
  });

  it('warns about the mix rate only for the shared backend', async () => {
    onInvoke('fork_hires_status', () => exclusiveStatus({ mixRateHz: 48000 }));
    const first = renderWithProviders(<ForkHiResEngineSection />);
    await screen.findByText('In use: Exclusive');
    expect(screen.queryByText(/Windows is mixing at/)).toBeNull();
    first.unmount();

    onInvoke('fork_hires_status', () => status({ mixRateHz: 48000 }));
    renderWithProviders(<ForkHiResEngineSection />);
    expect(await screen.findByText(/or switch Output to Exclusive\.$/)).toBeInTheDocument();
  });

  describe('polling', () => {
    beforeEach(() => { vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] }); });
    afterEach(() => { vi.useRealTimers(); });

    it('polls status every second and stops on unmount', async () => {
      onInvoke('fork_hires_status', () => status());
      const { unmount } = renderWithProviders(<ForkHiResEngineSection />);
      await act(async () => {});
      const base = statusCalls();

      await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
      expect(statusCalls()).toBe(base + 3);

      unmount();
      await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
      expect(statusCalls()).toBe(base + 3);
    });
  });
});

describe('loadForkHiResStatus', () => {
  it('syncs the shared enabled flag', async () => {
    onInvoke('fork_hires_status', () => status());
    await loadForkHiResStatus();
    expect(useForkHiResEnabled.getState().enabled).toBe(true);
  });
});
