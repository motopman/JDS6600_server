/// Write-back cache for the last known device register state.
///
/// Eliminates redundant UART writes when consecutive sequence blocks share
/// the same value for a parameter.  All fields are `Option<T>` so an
/// uninitialised cache never suppresses the first real write.
use crate::models::Waveform;

#[derive(Debug, Default)]
pub struct ChannelState {
    pub frequency_hz: Option<f64>,
    pub amplitude_v:  Option<f64>,
    pub offset_v:     Option<f64>,
    pub duty_percent: Option<f64>,
    pub waveform:     Option<Waveform>,
}

#[derive(Debug, Default)]
pub struct DeviceState {
    pub ch1: ChannelState,
    pub ch2: ChannelState,
    pub output_ch1: Option<bool>,
    pub output_ch2: Option<bool>,
}

impl DeviceState {
    pub fn new() -> Self { Self::default() }

    // ── Dirty checks (return true ↔ write needed) ─────────────────────

    pub fn frequency_changed(&self, ch: u8, hz: f64) -> bool {
        self.ch(ch).frequency_hz.map_or(true, |v| (v - hz).abs() > 0.005)
    }
    pub fn amplitude_changed(&self, ch: u8, v: f64) -> bool {
        self.ch(ch).amplitude_v.map_or(true, |c| (c - v).abs() > 0.001)
    }
    pub fn offset_changed(&self, ch: u8, v: f64) -> bool {
        self.ch(ch).offset_v.map_or(true, |c| (c - v).abs() > 0.001)
    }
    pub fn duty_changed(&self, ch: u8, pct: f64) -> bool {
        self.ch(ch).duty_percent.map_or(true, |c| (c - pct).abs() > 0.05)
    }
    pub fn waveform_changed(&self, ch: u8, wf: &Waveform) -> bool {
        self.ch(ch).waveform.as_ref().map_or(true, |c| c != wf)
    }
    pub fn output_changed(&self, ch1: bool, ch2: bool) -> bool {
        self.output_ch1.map_or(true, |v| v != ch1) ||
        self.output_ch2.map_or(true, |v| v != ch2)
    }

    // ── Write-back ────────────────────────────────────────────────────

    pub fn set_frequency(&mut self, ch: u8, hz: f64)   { self.ch_mut(ch).frequency_hz = Some(hz); }
    pub fn set_amplitude(&mut self, ch: u8, v: f64)    { self.ch_mut(ch).amplitude_v  = Some(v);  }
    pub fn set_offset   (&mut self, ch: u8, v: f64)    { self.ch_mut(ch).offset_v     = Some(v);  }
    pub fn set_duty     (&mut self, ch: u8, pct: f64)  { self.ch_mut(ch).duty_percent = Some(pct);}
    pub fn set_waveform (&mut self, ch: u8, wf: Waveform) { self.ch_mut(ch).waveform = Some(wf); }
    pub fn set_output   (&mut self, ch1: bool, ch2: bool) {
        self.output_ch1 = Some(ch1);
        self.output_ch2 = Some(ch2);
    }

    /// Invalidate all cached state – call after hardware reconnect so every
    /// register is re-written from the current sequence block.
    pub fn invalidate(&mut self) { *self = Self::default(); }

    fn ch(&self, ch: u8) -> &ChannelState {
        if ch == 2 { &self.ch2 } else { &self.ch1 }
    }
    fn ch_mut(&mut self, ch: u8) -> &mut ChannelState {
        if ch == 2 { &mut self.ch2 } else { &mut self.ch1 }
    }
}
