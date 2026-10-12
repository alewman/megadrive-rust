//! The consoles as data: one [`Machine`] per model, with its chips as variants.
//!
//! Transcribed from megadrive-python's `machine.py`. Every field that differs between
//! models names its source and that source's tier (megadrive-python
//! docs/validation.md, "The tier rule").
//!
//! Clocks: MAME 0.285 `sega/megadriv.h` (`MASTER_CLOCK_NTSC`/`_PAL`), agreeing with
//! the Manual's CPU clocks (docs/start-here.md, "Clocks").
//!
//! Rust's difference: the reference's `MACHINES` dict is [`machine`] and
//! [`MACHINE_NAMES`], and its frozen dataclasses are `Copy` structs.

// The FM chip's two variants are the core's (ym2612-python, docs/contract.md, "One
// chip, two variants"); a Machine names one.
pub use ym2612_rust::chip::{FMVariant, YM2612_DISCRETE, YM3438_ASIC};

pub const MCLK_NTSC: i64 = 53_693_175;
pub const MCLK_PAL: i64 = 53_203_424;

/// Master clocks per 68000 clock and per Z80 clock (docs/timing.md).
pub const M68K_DIVIDER: i64 = 7;
pub const Z80_DIVIDER: i64 = 15;

/// A line is 3,420 MCLK in both horizontal modes (Jorge Nuno's oscilloscope, forum
/// t=519; Kabuto, "Picture size"). docs/timing.md, "The line and the frame".
pub const MCLK_PER_LINE: i64 = 3420;

/// The 315-5313 (YM7101) as one model has it.
///
/// `vsram_bytes`: 0x50 (40 words) on the Model 1 and Model 2; 0x80 on the Genesis 3,
/// which is why it fails VDPFIFOTesting's VSRAM tests ("it actually has an 0x80 byte
/// VSRAM, not a 0x50 byte VSRAM like earlier VDP models": Nemesis, forum t=1291
/// start=30; hardware-corrected).
///
/// `revision`: "315-5313" or "315-5313A", the manufacturing change behind Overdrive
/// 2's glitches on ~30 % of Model 1s ("VA4 and prior use this chip, while VA6 and
/// higher use the 5313A or newer", Kabuto, "Plane masking instability"; research).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VDPVariant {
    pub name: &'static str,
    pub vsram_bytes: usize,
    pub revision: &'static str,
}

pub const VDP_5313: VDPVariant = VDPVariant {
    name: "315-5313",
    vsram_bytes: 0x50,
    revision: "315-5313",
};
pub const VDP_5313A: VDPVariant = VDPVariant {
    name: "315-5313A",
    vsram_bytes: 0x50,
    revision: "315-5313A",
};
pub const VDP_GENESIS3: VDPVariant = VDPVariant {
    name: "Genesis 3 (FF1004 VDP)",
    vsram_bytes: 0x80,
    revision: "Genesis 3",
};

/// One console model.
///
/// `overseas` and `pal` are bits 7 and 6 of the version register $A10001 (Manual PDF
/// page 84); `hardware_version` its bits 3-0: 0 on consoles without TMSS, nonzero on
/// those with it (from Model 1 VA6; Manual PDF page 94, TmEE's dumps in forum t=3197).
/// `expansion` is an attached expansion unit (the Mega CD), which clears bit 5.
/// `audio_path` names the analogue mixer ("model1": a first-order filter; "model2":
/// second-order and more distortion; Kabuto, measured on several boards; research).
///
/// `hv_unit` names the measured console whose HV counter reads a machine copies at the
/// one sampling phase where a read catches the counters changing (`vdp::HV_UNITS`):
/// Nemesis's PAL Model 1 or his NTSC-J Model 1. The two differ there, by analogue
/// timing; which other consoles behave like either is not known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Machine {
    pub name: &'static str,
    pub pal: bool,
    pub overseas: bool,
    pub vdp: VDPVariant,
    pub fm: FMVariant,
    pub tmss: bool,
    pub hardware_version: u8,
    pub audio_path: &'static str,
    pub expansion: bool,
    pub hv_unit: &'static str,
    /// Where Kabuto's hardware-measured DMA figures and Nuked-MD's disagree -- a
    /// transfer's start-up (5.6 against 8.0 68000 cycles) and the fast pace (2.4
    /// against 2.47 cycles a word) -- whose the board uses: "kabuto", the tier rule's
    /// choice, or "die", for comparisons against Nuked-MD (Aubrey's option 3;
    /// docs/worklog.md, "Kabuto's figures or the die's").
    pub dma_figures: &'static str,
}

impl Machine {
    pub fn hv_reads(&self) -> &'static str {
        if !self.hv_unit.is_empty() {
            self.hv_unit
        } else if self.pal {
            "pal-model1"
        } else {
            "ntscj-model1"
        }
    }

    pub fn mclk(&self) -> i64 {
        if self.pal {
            MCLK_PAL
        } else {
            MCLK_NTSC
        }
    }

    /// 262 NTSC, 313 PAL (Kabuto, "Picture size"; docs/timing.md).
    pub fn lines_per_frame(&self) -> i64 {
        if self.pal {
            313
        } else {
            262
        }
    }

    /// $A10001: overseas, PAL, no expansion unit, hardware version (Manual p. 84).
    pub fn version_register(&self) -> u8 {
        (if self.overseas { 0x80 } else { 0 })
            | (if self.pal { 0x40 } else { 0 })
            | (if self.expansion { 0 } else { 0x20 })
            | (self.hardware_version & 0x0F)
    }
}

#[allow(clippy::too_many_arguments)]
const fn model(
    name: &'static str,
    pal: bool,
    overseas: bool,
    vdp: VDPVariant,
    fm: FMVariant,
    tmss: bool,
    hardware_version: u8,
    audio_path: &'static str,
) -> Machine {
    Machine {
        name,
        pal,
        overseas,
        vdp,
        fm,
        tmss,
        hardware_version,
        audio_path,
        expansion: false,
        hv_unit: "",
        dma_figures: "kabuto",
    }
}

// The Model 1 before VA6: no TMSS, version 0, the 315-5313, the discrete YM2612.
const fn model1_early(name: &'static str, pal: bool, overseas: bool) -> Machine {
    model(
        name,
        pal,
        overseas,
        VDP_5313,
        YM2612_DISCRETE,
        false,
        0,
        "model1",
    )
}
// The Model 1 from VA6: TMSS and a nonzero version; the 315-5313A.
const fn model1_tmss(name: &'static str, pal: bool, overseas: bool) -> Machine {
    model(
        name,
        pal,
        overseas,
        VDP_5313A,
        YM2612_DISCRETE,
        true,
        1,
        "model1",
    )
}
// The Model 2 (from its VA1 on; its earliest boards keep the discrete YM2612 -- a
// variant a caller builds by changing `fm`): TMSS, the ASIC FM.
const fn model2(name: &'static str, pal: bool, overseas: bool) -> Machine {
    model(
        name,
        pal,
        overseas,
        VDP_5313A,
        YM3438_ASIC,
        true,
        1,
        "model2",
    )
}

/// The machines by the reference's names, in its dict's order.
pub const MACHINE_NAMES: [&str; 10] = [
    "genesis",
    "genesis-tmss",
    "genesis2",
    "genesis3",
    "megadrive",
    "megadrive-tmss",
    "megadrive2",
    "megadrive-japan",
    "megadrive2-japan",
    "nuked-md",
];

/// The reference's `MACHINES[name]`.
pub fn machine(name: &str) -> Option<Machine> {
    Some(match name {
        // MAME's `genesis` driver: export NTSC, no TMSS (the lockstep's machine).
        "genesis" => model1_early("Genesis (Model 1, no TMSS)", false, true),
        "genesis-tmss" => model1_tmss("Genesis (Model 1 VA6+, TMSS)", false, true),
        "genesis2" => model2("Genesis 2 (Model 2)", false, true),
        "genesis3" => Machine {
            vdp: VDP_GENESIS3,
            ..model2("Genesis 3", false, true)
        },
        "megadrive" => model1_early("Mega Drive (Model 1, PAL)", true, true),
        "megadrive-tmss" => model1_tmss("Mega Drive (Model 1 VA6+, PAL)", true, true),
        "megadrive2" => model2("Mega Drive 2 (PAL)", true, true),
        "megadrive-japan" => model1_early("Mega Drive (Model 1, Japan)", false, false),
        "megadrive2-japan" => model2("Mega Drive 2 (Japan)", false, false),
        // The die model's console -- a PAL Model 1 with TMSS (its version register
        // reads 1) -- with its DMA figures where they and Kabuto's disagree
        // (`Machine::dma_figures`).
        "nuked-md" => Machine {
            name: "Nuked-MD (Model 1 VA6+, PAL, the die's DMA figures)",
            dma_figures: "die",
            ..model1_tmss("Mega Drive (Model 1 VA6+, PAL)", true, true)
        },
        _ => return None,
    })
}

/// The machine each MAME 0.285 driver emulates (sega/mdconsole.cpp; version nibbles
/// from megadriv.cpp `init_megadriv`, `init_megadrij`, `init_megadrie`).
pub const MAME_DRIVERS: [(&str, &str); 4] = [
    ("genesis", "genesis"),
    ("megadriv", "megadrive"),
    ("megadrij", "megadrive-japan"),
    ("genesis_tmss", "genesis-tmss"),
];
