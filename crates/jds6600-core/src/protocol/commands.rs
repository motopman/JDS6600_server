/// JDS6600 ASCII command encoder.
///
/// ## Protocol wire format
/// | Direction | Frame                     |
/// |-----------|---------------------------|
/// | Read      | `:rXX.\r\n`               |
/// | Write     | `:wXX=VALUE.\r\n`          |
/// | Response  | `OK\r\n` or `:rXX=VALUE.` |
///
/// ## Function-code map  (update after `jdsctl sweep`)
/// | Code | Parameter          | Unit              |
/// |------|--------------------|-------------------|
/// | 00   | Device model ID    | —                 |
/// | 20   | Output enable      | bit0=CH1 bit1=CH2 |
/// | 23   | CH1 Frequency      | 0.01 Hz           |
/// | 24   | CH2 Frequency      | 0.01 Hz           |
/// | 25   | CH1 Waveform index | integer           |
/// | 26   | CH2 Waveform index | integer           |
/// | 27   | CH1 Amplitude      | mV                |
/// | 28   | CH2 Amplitude      | mV                |
/// | 29   | CH1 Offset         | mV + 10 000 bias  |
/// | 30   | CH2 Offset         | mV + 10 000 bias  |
/// | 31   | CH1 Duty           | 0.1 %             |
/// | 32   | CH2 Duty           | 0.1 %             |

use crate::error::{JdsError, JdsResult};
use crate::models::Waveform;

// ── Command enum ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum GeneratorCommand {
    SetFrequency { channel: u8, hz: f64 },
    SetAmplitude { channel: u8, volts: f64 },
    SetOffset    { channel: u8, volts: f64 },
    SetDuty      { channel: u8, percent: f64 },
    SetWaveform  { channel: u8, waveform: Waveform },
    SetOutputEnable { ch1: bool, ch2: bool },
}

// ── Encoder ───────────────────────────────────────────────────────────────

/// Encode `cmd` into a `<CR><LF>`-terminated ASCII frame.
/// Returns `Err(OutOfBounds)` for values outside the device's safe range.
pub fn build_ascii_command(cmd: &GeneratorCommand) -> JdsResult<Vec<u8>> {
    let frame = match cmd {
        GeneratorCommand::SetFrequency { channel, hz } => {
            validate("frequency", *hz, 0.01, 60_000_000.0)?;
            // Unit: 0.01 Hz → multiply × 100
            let raw = (*hz * 100.0).round() as u64;
            format!(":w{:02}={},0.\r\n", freq_code(*channel)?, raw)
        }
        GeneratorCommand::SetAmplitude { channel, volts } => {
            validate("amplitude", *volts, 0.0, 20.0)?;
            let raw = (*volts * 1000.0).round() as u32;
            format!(":w{:02}={}.\r\n", amp_code(*channel)?, raw)
        }
        GeneratorCommand::SetOffset { channel, volts } => {
            validate("offset", *volts, -10.0, 10.0)?;
            // Bias: +10 000 mV keeps the value unsigned
            let raw = ((*volts * 1000.0).round() as i32 + 10_000) as u32;
            format!(":w{:02}={}.\r\n", offset_code(*channel)?, raw)
        }
        GeneratorCommand::SetDuty { channel, percent } => {
            validate("duty", *percent, 0.0, 99.9)?;
            // Unit: 0.1 % → multiply × 10
            let raw = (*percent * 10.0).round() as u32;
            format!(":w{:02}={}.\r\n", duty_code(*channel)?, raw)
        }
        GeneratorCommand::SetWaveform { channel, waveform } => {
            format!(":w{:02}={}.\r\n", wave_code(*channel)?, waveform.to_device_index())
        }
        GeneratorCommand::SetOutputEnable { ch1, ch2 } => {
            let bits: u8 = (*ch1 as u8) | ((*ch2 as u8) << 1);
            format!(":w20={}.\r\n", bits)
        }
    };
    Ok(frame.into_bytes())
}

// ── Function-code helpers ─────────────────────────────────────────────────

fn freq_code(ch: u8)   -> JdsResult<u8> { ch_code(ch, 23, 24) }
fn wave_code(ch: u8)   -> JdsResult<u8> { ch_code(ch, 25, 26) }
fn amp_code(ch: u8)    -> JdsResult<u8> { ch_code(ch, 27, 28) }
fn offset_code(ch: u8) -> JdsResult<u8> { ch_code(ch, 29, 30) }
fn duty_code(ch: u8)   -> JdsResult<u8> { ch_code(ch, 31, 32) }

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

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(cmd: &GeneratorCommand) -> String {
        String::from_utf8(build_ascii_command(cmd).unwrap()).unwrap()
    }

    #[test]
    fn freq_1khz() {
        // 1000 Hz × 100 = 100 000
        assert_eq!(s(&GeneratorCommand::SetFrequency { channel: 1, hz: 1000.0 }), ":w23=100000,0.\r\n");
    }

    #[test]
    fn freq_fractional() {
        // 0.5 Hz × 100 = 50
        assert_eq!(s(&GeneratorCommand::SetFrequency { channel: 1, hz: 0.5 }), ":w23=50,0.\r\n");
    }

    #[test]
    fn amplitude_3v3() {
        // 3.3 V → 3300 mV
        assert_eq!(s(&GeneratorCommand::SetAmplitude { channel: 1, volts: 3.3 }), ":w27=3300.\r\n");
    }

    #[test]
    fn offset_negative() {
        // -1 V → -1000 mV + 10000 bias = 9000
        assert_eq!(s(&GeneratorCommand::SetOffset { channel: 1, volts: -1.0 }), ":w29=9000.\r\n");
    }

    #[test]
    fn offset_zero() {
        // 0 V → 0 mV + 10000 bias = 10000
        assert_eq!(s(&GeneratorCommand::SetOffset { channel: 1, volts: 0.0 }), ":w29=10000.\r\n");
    }

    #[test]
    fn duty_50pct() {
        // 50.0 % × 10 = 500
        assert_eq!(s(&GeneratorCommand::SetDuty { channel: 1, percent: 50.0 }), ":w31=500.\r\n");
    }

    #[test]
    fn output_both_on() {
        assert_eq!(s(&GeneratorCommand::SetOutputEnable { ch1: true, ch2: true }), ":w20=3.\r\n");
    }

    #[test]
    fn output_ch1_only() {
        assert_eq!(s(&GeneratorCommand::SetOutputEnable { ch1: true, ch2: false }), ":w20=1.\r\n");
    }

    #[test]
    fn output_both_off() {
        assert_eq!(s(&GeneratorCommand::SetOutputEnable { ch1: false, ch2: false }), ":w20=0.\r\n");
    }

    #[test]
    fn ch2_freq() {
        assert_eq!(s(&GeneratorCommand::SetFrequency { channel: 2, hz: 500.0 }), ":w24=50000,0.\r\n");
    }

    #[test]
    fn invalid_channel() {
        assert!(build_ascii_command(&GeneratorCommand::SetFrequency { channel: 3, hz: 1.0 }).is_err());
    }

    #[test]
    fn freq_out_of_range() {
        assert!(build_ascii_command(&GeneratorCommand::SetFrequency { channel: 1, hz: 1e9 }).is_err());
    }

    #[test]
    fn waveform_square() {
        let cmd = GeneratorCommand::SetWaveform { channel: 1, waveform: Waveform::Square };
        assert_eq!(s(&cmd), ":w25=1.\r\n");
    }
}
