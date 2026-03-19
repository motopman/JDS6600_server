/// JDS6600 ASCII command encoder.
///
/// ## Wire format  (from official JDS6600 communication specification)
/// | Direction | Frame                          |
/// |-----------|--------------------------------|
/// | Write     | `:wXX=VALUE.\r\n`              |
/// | Read      | `:rXX=0.\r\n`                  |
/// | Response  | `OK\r\n`  (write)              |
/// | Response  | `:rXX=VALUE.\r\n`  (read)      |
///
/// ## Verified function-code map  (from Joy-IT JDS6600 manual)
/// | Code | CH1  | CH2  | Parameter        | Unit / encoding              |
/// |------|------|------|------------------|------------------------------|
/// | 00   |      |      | Device model ID  | read-only                    |
/// | 20   |      |      | Output enable    | bit0=CH1, bit1=CH2           |
/// | 21   |  ✓   |      | CH1 Waveform     | index (0=sine,1=sq,2=tri…)   |
/// | 22   |      |  ✓   | CH2 Waveform     | same index                   |
/// | 23   |  ✓   |      | CH1 Frequency    | 0.01 Hz/unit, unit field=0   |
/// | 24   |      |  ✓   | CH2 Frequency    | same                         |
/// | 25   |  ✓   |      | CH1 Amplitude    | mV  (x=30 → 0.03 V)         |
/// | 26   |      |  ✓   | CH2 Amplitude    | mV                           |
/// | 27   |  ✓   |      | CH1 Bias/Offset  | 1000=0V, +1=+0.01V, -1=-0.01V|
/// | 28   |      |  ✓   | CH2 Bias/Offset  | same                         |
/// | 29   |  ✓   |      | CH1 Duty cycle   | 0.1 % unit (500 → 50%)       |
/// | 30   |      |  ✓   | CH2 Duty cycle   | same                         |
/// | 31   |      |      | Phase            | 0.1° unit (100 → 10°)        |
/// | 54   |      |      | Tracking/sync    | multi-field                  |
///
/// ## Bias/Offset encoding  (verified from manual examples)
/// raw = round(volts × 100) + 1000
/// • value   1 → −9.99 V   (1 − 1000) / 100 = −9.99
/// • value 1000 →  0.00 V
/// • value 1999 → +9.99 V
///
/// ## Frequency encoding
/// raw = round(hz × 100), second field = 0 (Hz unit)
/// Unit field: 0=Hz, 1=kHz, 2=MHz, 3=mHz, 4=µHz

use crate::error::{JdsError, JdsResult};
use crate::models::Waveform;

// ── Read helper (used by scanner and HAL probe) ───────────────────────────

/// Format a read command: `:rXX=0.\r\n`
pub fn build_read_command(code: u8) -> Vec<u8> {
    format!(":r{:02}=0.\r\n", code).into_bytes()
}

// ── Command enum ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum GeneratorCommand {
    SetFrequency    { channel: u8, hz: f64 },
    SetAmplitude    { channel: u8, volts: f64 },
    /// Bias / DC offset.  Range: −9.99 V .. +9.99 V.
    SetOffset       { channel: u8, volts: f64 },
    SetDuty         { channel: u8, percent: f64 },
    SetWaveform     { channel: u8, waveform: Waveform },
    SetOutputEnable { ch1: bool, ch2: bool },
}

// ── Encoder ───────────────────────────────────────────────────────────────

/// Encode a domain command into a `<CR><LF>`-terminated ASCII frame.
/// Returns `Err(OutOfBounds)` when a value is outside the device's range.
pub fn build_ascii_command(cmd: &GeneratorCommand) -> JdsResult<Vec<u8>> {
    let frame = match cmd {

        // ── Frequency ─────────────────────────────────────────────────
        // Format:  :w23=VALUE,0.\r\n
        // Unit:    0.01 Hz/unit  →  raw = hz × 100
        // Second field (0) = Hz unit selector
        GeneratorCommand::SetFrequency { channel, hz } => {
            validate("frequency", *hz, 0.01, 60_000_000.0)?;
            let raw = (*hz * 100.0).round() as u64;
            format!(":w{:02}={},0.\r\n", freq_code(*channel)?, raw)
        }

        // ── Amplitude ─────────────────────────────────────────────────
        // Format:  :w25=VALUE.\r\n
        // Unit:    mV  (x=30 → 0.03 V)  →  raw = volts × 1000
        GeneratorCommand::SetAmplitude { channel, volts } => {
            validate("amplitude", *volts, 0.0, 20.0)?;
            let raw = (*volts * 1000.0).round() as u32;
            format!(":w{:02}={}.\r\n", amp_code(*channel)?, raw)
        }

        // ── Bias / Offset ─────────────────────────────────────────────
        // Format:  :w27=VALUE.\r\n
        // Encoding: raw = round(volts × 100) + 1000
        //   raw 1000 = 0 V, raw 1999 = +9.99 V, raw 1 = −9.99 V
        GeneratorCommand::SetOffset { channel, volts } => {
            validate("bias", *volts, -9.99, 9.99)?;
            let raw = ((*volts * 100.0).round() as i32 + 1000) as u32;
            format!(":w{:02}={}.\r\n", offset_code(*channel)?, raw)
        }

        // ── Duty cycle ────────────────────────────────────────────────
        // Format:  :w29=VALUE.\r\n
        // Unit:    0.1 %/unit  →  raw = percent × 10  (500 → 50%)
        GeneratorCommand::SetDuty { channel, percent } => {
            validate("duty", *percent, 0.0, 99.9)?;
            let raw = (*percent * 10.0).round() as u32;
            format!(":w{:02}={}.\r\n", duty_code(*channel)?, raw)
        }

        // ── Waveform ──────────────────────────────────────────────────
        // Format:  :w21=INDEX.\r\n
        GeneratorCommand::SetWaveform { channel, waveform } => {
            format!(":w{:02}={}.\r\n", wave_code(*channel)?, waveform.to_device_index())
        }

        // ── Output enable ─────────────────────────────────────────────
        // Format:  :w20=CH1,CH2.\r\n
        //   :w20=1,1.  both on    :w20=0,0.  both off
        //   :w20=1,0.  ch1 only   :w20=0,1.  ch2 only
        GeneratorCommand::SetOutputEnable { ch1, ch2 } => {
            format!(":w20={},{}.\r\n", *ch1 as u8, *ch2 as u8)
        }
    };
    Ok(frame.into_bytes())
}

// ── Function-code table ───────────────────────────────────────────────────

fn freq_code  (ch: u8) -> JdsResult<u8> { ch_code(ch, 23, 24) }
fn wave_code  (ch: u8) -> JdsResult<u8> { ch_code(ch, 21, 22) }  // manual §waveform
fn amp_code   (ch: u8) -> JdsResult<u8> { ch_code(ch, 25, 26) }  // manual §range
fn offset_code(ch: u8) -> JdsResult<u8> { ch_code(ch, 27, 28) }  // manual §bias
fn duty_code  (ch: u8) -> JdsResult<u8> { ch_code(ch, 29, 30) }  // manual §duty

fn ch_code(ch: u8, code1: u8, code2: u8) -> JdsResult<u8> {
    match ch {
        1 => Ok(code1),
        2 => Ok(code2),
        _ => Err(JdsError::Protocol(format!("Invalid channel: {}", ch))),
    }
}

fn validate(field: &str, v: f64, min: f64, max: f64) -> JdsResult<()> {
    if v < min || v > max {
        Err(JdsError::OutOfBounds { field: field.into(), value: v, min, max })
    } else {
        Ok(())
    }
}

// ── Tests — every value verified against the manual ──────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(cmd: &GeneratorCommand) -> String {
        String::from_utf8(build_ascii_command(cmd).unwrap()).unwrap()
    }

    // ── Frequency ──────────────────────────────────────────────────────

    #[test]
    fn freq_1khz_ch1() {
        // 1000 Hz × 100 = 100 000; unit field = 0 (Hz)
        assert_eq!(s(&GeneratorCommand::SetFrequency { channel: 1, hz: 1000.0 }),
                   ":w23=100000,0.\r\n");
    }

    #[test]
    fn freq_257hz86() {
        // Manual example: 257.86 Hz → value 25786
        assert_eq!(s(&GeneratorCommand::SetFrequency { channel: 1, hz: 257.86 }),
                   ":w23=25786,0.\r\n");
    }

    #[test]
    fn freq_fractional() {
        // 0.5 Hz × 100 = 50
        assert_eq!(s(&GeneratorCommand::SetFrequency { channel: 1, hz: 0.5 }),
                   ":w23=50,0.\r\n");
    }

    #[test]
    fn freq_ch2() {
        assert_eq!(s(&GeneratorCommand::SetFrequency { channel: 2, hz: 500.0 }),
                   ":w24=50000,0.\r\n");
    }

    // ── Amplitude ──────────────────────────────────────────────────────

    #[test]
    fn amp_3v3() {
        // 3.3 V → 3300 mV; code 25 = CH1 amplitude
        assert_eq!(s(&GeneratorCommand::SetAmplitude { channel: 1, volts: 3.3 }),
                   ":w25=3300.\r\n");
    }

    #[test]
    fn amp_30mv() {
        // Manual example: x=30 → 0.03 V
        assert_eq!(s(&GeneratorCommand::SetAmplitude { channel: 1, volts: 0.03 }),
                   ":w25=30.\r\n");
    }

    #[test]
    fn amp_ch2() {
        assert_eq!(s(&GeneratorCommand::SetAmplitude { channel: 2, volts: 5.0 }),
                   ":w26=5000.\r\n");
    }

    // ── Bias / Offset ──────────────────────────────────────────────────

    #[test]
    fn bias_zero() {
        // Manual: :w27=1000. → bias 0 V
        assert_eq!(s(&GeneratorCommand::SetOffset { channel: 1, volts: 0.0 }),
                   ":w27=1000.\r\n");
    }

    #[test]
    fn bias_positive_9v99() {
        // Manual: value 1999 → +9.99 V  (1999 − 1000) / 100 = +9.99
        assert_eq!(s(&GeneratorCommand::SetOffset { channel: 1, volts: 9.99 }),
                   ":w27=1999.\r\n");
    }

    #[test]
    fn bias_negative_9v99() {
        // Manual: :w27=1. → bias −9.99 V  (1 − 1000) / 100 = −9.99
        assert_eq!(s(&GeneratorCommand::SetOffset { channel: 1, volts: -9.99 }),
                   ":w27=1.\r\n");
    }

    #[test]
    fn bias_minus_1v() {
        // (−1.0 × 100) + 1000 = 900
        assert_eq!(s(&GeneratorCommand::SetOffset { channel: 1, volts: -1.0 }),
                   ":w27=900.\r\n");
    }

    #[test]
    fn bias_ch2() {
        assert_eq!(s(&GeneratorCommand::SetOffset { channel: 2, volts: 0.0 }),
                   ":w28=1000.\r\n");
    }

    // ── Duty cycle ─────────────────────────────────────────────────────

    #[test]
    fn duty_50pct() {
        // Manual: x=500 → 50%
        assert_eq!(s(&GeneratorCommand::SetDuty { channel: 1, percent: 50.0 }),
                   ":w29=500.\r\n");
    }

    #[test]
    fn duty_ch2() {
        assert_eq!(s(&GeneratorCommand::SetDuty { channel: 2, percent: 25.0 }),
                   ":w30=250.\r\n");
    }

    // ── Waveform ───────────────────────────────────────────────────────

    #[test]
    fn waveform_sine_ch1() {
        // code 21, index 0
        let cmd = GeneratorCommand::SetWaveform { channel: 1, waveform: Waveform::Sine };
        assert_eq!(s(&cmd), ":w21=0.\r\n");
    }

    #[test]
    fn waveform_square_ch1() {
        // code 21, index 1
        let cmd = GeneratorCommand::SetWaveform { channel: 1, waveform: Waveform::Square };
        assert_eq!(s(&cmd), ":w21=1.\r\n");
    }

    #[test]
    fn waveform_ch2() {
        // code 22
        let cmd = GeneratorCommand::SetWaveform { channel: 2, waveform: Waveform::Triangle };
        assert_eq!(s(&cmd), ":w22=2.\r\n");
    }

    // ── Output enable ──────────────────────────────────────────────────

    #[test]
    fn output_both_on()  { assert_eq!(s(&GeneratorCommand::SetOutputEnable { ch1: true,  ch2: true  }), ":w20=1,1.\r\n"); }
    #[test]
    fn output_ch1_only() { assert_eq!(s(&GeneratorCommand::SetOutputEnable { ch1: true,  ch2: false }), ":w20=1,0.\r\n"); }
    #[test]
    fn output_ch2_only() { assert_eq!(s(&GeneratorCommand::SetOutputEnable { ch1: false, ch2: true  }), ":w20=0,1.\r\n"); }
    #[test]
    fn output_both_off() { assert_eq!(s(&GeneratorCommand::SetOutputEnable { ch1: false, ch2: false }), ":w20=0,0.\r\n"); }

    // ── Read command helper ────────────────────────────────────────────

    #[test]
    fn read_cmd_format() {
        // Protocol requires :rXX=0.\r\n  NOT :rXX.\r\n
        let bytes = build_read_command(23);
        assert_eq!(String::from_utf8(bytes).unwrap(), ":r23=0.\r\n");
    }

    // ── Validation ─────────────────────────────────────────────────────

    #[test]
    fn invalid_channel() {
        assert!(build_ascii_command(&GeneratorCommand::SetFrequency { channel: 3, hz: 1.0 }).is_err());
    }

    #[test]
    fn freq_out_of_range() {
        assert!(build_ascii_command(&GeneratorCommand::SetFrequency { channel: 1, hz: 1e9 }).is_err());
    }

    #[test]
    fn bias_out_of_range() {
        assert!(build_ascii_command(&GeneratorCommand::SetOffset { channel: 1, volts: 15.0 }).is_err());
    }
}
