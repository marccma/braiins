extern crate libc;
extern crate nix;
extern crate s9_io;

use self::nix::sys::mman::{MapFlags, ProtFlags};
use core;

use std::fs::OpenOptions;
use std::io;
use std::mem::size_of;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
// TODO: remove thread specific components
use std::thread;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use uint;

use byteorder::{ByteOrder, LittleEndian};
use packed_struct::{PackedStruct, PackedStructSlice};

use embedded_hal::digital::InputPin;
use embedded_hal::digital::OutputPin;
use linux_embedded_hal::I2cdev;

use self::s9_io::hchainio0;

mod bm1387;
pub mod gpio;
pub mod power;

/// Timing constants
const INACTIVATE_FROM_CHAIN_DELAY_MS: u64 = 100;
/// Base delay quantum during hashboard initialization
const INIT_DELAY_MS: u64 = 1000;

/// Maximum number of chips is limitted by the fact that there is only 8-bit address field and
/// addresses to the chips need to be assigned with step of 4 (e.g. 0, 4, 8, etc.)
const MAX_CHIPS_ON_CHAIN: usize = 64;

/// Bit position where work ID starts in the second word provided by the IP core with mining work
/// result
const WORK_ID_OFFSET: usize = 8;

/// Hash Chain Controller provides abstraction of the FPGA interface for operating hashing boards.
/// It is the user-space driver for the IP Core
///
/// Main responsibilities:
/// - memory mapping of the FPGA control interface
/// - mining work submission and result processing
///
/// TODO: implement drop trait (results in unmap)
/// TODO: rename to HashBoardCtrl and get rid of the hash_chain identifiers + array
pub struct HChainCtl<'a, 'b> {
    hash_chain_ios: [&'a hchainio0::RegisterBlock; 2],
    /// Current work ID once it rolls over, we can start retiring old jobs
    work_id: u16,
    /// Number of chips that have been detected
    chip_count: usize,
    /// Eliminates the need to query the IP core about the current number of configured midstates
    midstate_count_bits: u8,
    /// Voltage controller on this hashboard
    voltage_ctrl: power::VoltageCtrl<'b>,
    /// Plug pin that indicates the hashboard is present
    plug_pin: gpio::PinIn,
    /// Pin for resetting the hashboard
    rst_pin: gpio::PinOut,
    /// When the heartbeat was last sent
    last_heartbeat_sent: Option<SystemTime>,
}

impl<'a, 'b> HChainCtl<'a, 'b> {
    /// Performs memory mapping of IP core's register block
    /// # TODO
    /// Research why custom flags - specifically O_SYNC and O_LARGEFILE fail
    fn mmap() -> Result<*const hchainio0::RegisterBlock, io::Error> {
        let mem_file = //File::open(path)?;
            OpenOptions::new().read(true).write(true)
                //.custom_flags(libc::O_RDWR | libc::O_SYNC | libc::O_LARGEFILE)
                .open("/dev/mem")?;

        let mmap = unsafe {
            nix::sys::mman::mmap(
                0 as *mut libc::c_void,
                4096,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                mem_file.as_raw_fd(),
                s9_io::HCHAINIO0::ptr() as libc::off_t,
            )
        };
        mmap.map(|addr| addr as *const hchainio0::RegisterBlock)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("mmap error! {:?}", e)))
    }

    /// Creates a new hashboard controller with memory mapped FPGA IP core
    ///
    /// * `gpio_mgr` - gpio manager used for producing pins required for hashboard control
    /// * `voltage_ctrl_backend` - communication backend for the voltage controller
    /// * `hashboard_idx` - index of this hashboard determines which FPGA IP core is to be mapped
    /// * `midstate_count` - see Self
    pub fn new(
        gpio_mgr: &gpio::ControlPinManager,
        voltage_ctrl_backend: &'b mut power::VoltageCtrlBackend,
        hashboard_idx: usize,
        midstate_count: &s9_io::hchainio0::ctrl_reg::MIDSTATE_CNTW,
    ) -> Result<Self, io::Error> {
        // Hashboard creation is aborted if the pin is not present
        let plug_pin = gpio_mgr
            .get_pin_in(gpio::PinInName::Plug(hashboard_idx))
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "Hashboard {} failed to initialize plug pin: {}",
                        hashboard_idx, e
                    ),
                )
            })?;
        // also detect that the board is present
        if plug_pin.is_low() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Hashboard {} not present", hashboard_idx),
            ));
        }

        // Instantiate the reset pin
        let rst_pin = gpio_mgr
            .get_pin_out(gpio::PinOutName::Rst(hashboard_idx))
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "Hashboard {}: failed to initialize reset pin: {}",
                        hashboard_idx, e
                    ),
                )
            })?;

        let hash_chain_io = Self::mmap()?;
        let hash_chain_io = unsafe { &*hash_chain_io };

        Result::Ok(Self {
            hash_chain_ios: [hash_chain_io, hash_chain_io],
            work_id: 0,
            chip_count: 0,
            midstate_count_bits: midstate_count._bits(),
            voltage_ctrl: power::VoltageCtrl::new(voltage_ctrl_backend, hashboard_idx),
            plug_pin,
            rst_pin,
            last_heartbeat_sent: None,
        })
    }

    /// Helper method that initializes the FPGA IP core
    fn ip_core_init(&self) -> Result<(), io::Error> {
        // Disable ip core
        self.disable_ip_core();
        self.enable_ip_core();

        self.set_baud(115200);
        // TODO consolidate hardcoded constant - calculate time constant based on PLL settings etc.
        self.set_work_time(50000);
        self.set_midstate_count();

        Ok(())
    }

    /// Puts the board into reset mode and disables the associated IP core
    fn enter_reset(&mut self) {
        self.disable_ip_core();
        // perform reset of the hashboard
        self.rst_pin.set_low();
    }

    /// Leaves reset mode
    fn exit_reset(&mut self) {
        self.rst_pin.set_high();
        self.enable_ip_core();
    }

    /// Helper method that sends heartbeat to the voltage controller so that it knows we are
    /// alive and won't shutdown powersupply to the hashboard.
    /// At the same time, this method takes care of sending the heart beat only every N milliseconds
    fn send_voltage_ctrl_heart_beat(&mut self) -> Result<(), io::Error> {
        if self.last_heartbeat_sent == None {
            self.last_heartbeat_sent = Some(SystemTime::now());
        }
        if let Some(t) = self.last_heartbeat_sent {
            let elapsed = t.elapsed().map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("System time error: {}", e))
            })?;
            if elapsed >= Duration::from_secs(1) {
                self.last_heartbeat_sent = Some(SystemTime::now());
                return self.voltage_ctrl.send_heart_beat();
            }
        }
        Ok(())
    }

    /// Initializes the complete hashboard including enumerating all chips
    pub fn init(&mut self) -> Result<(), io::Error> {
        self.ip_core_init()?;

        self.voltage_ctrl.reset()?;
        self.voltage_ctrl.jump_from_loader_to_app()?;

        let version = self.voltage_ctrl.get_version()?;
        // TODO accept multiple
        if version != power::EXPECTED_VOLTAGE_CTRL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Unexpected voltage controller firmware version: {}, expected: {}",
                    version,
                    power::EXPECTED_VOLTAGE_CTRL_VERSION
                ),
            ));
        }

        self.voltage_ctrl.set_voltage(6)?;
        self.voltage_ctrl.enable_voltage()?;

        self.enter_reset();
        // disable voltage
        self.voltage_ctrl.disable_voltage()?;
        thread::sleep(Duration::from_millis(INIT_DELAY_MS));
        self.voltage_ctrl.enable_voltage()?;
        thread::sleep(Duration::from_millis(2 * INIT_DELAY_MS));
        // TODO remove once we have a dedicated heartbeat task
        self.send_voltage_ctrl_heart_beat()?;

        // TODO consider including a delay
        self.exit_reset();
        thread::sleep(Duration::from_millis(INIT_DELAY_MS));
        //        let voltage = self.voltage_ctrl.get_voltage()?;
        //        if voltage != 0 {
        //            return Err(io::Error::new(
        //                io::ErrorKind::Other, format!("Detected voltage {}", voltage)));
        //        }
        self.enumerate_chips()?;
        println!("Discovered {} chips", self.chip_count);

        // set PLL
        self.set_pll()?;

        // enable hashing chain
        self.configure_hash_chain()?;

        Ok(())
    }

    /// Detects the number of chips on the hashing chain and assigns an address to each chip
    fn enumerate_chips(&mut self) -> Result<(), io::Error> {
        // Enumerate all chips (broadcast read address register request)
        let get_addr_cmd = bm1387::GetStatusCmd::new(0, true, bm1387::GET_ADDRESS_REG).pack();
        self.send_ctl_cmd(&get_addr_cmd, false);
        self.chip_count = 0;
        while let Some(addr_reg) = self.recv_ctl_cmd_resp::<bm1387::GetAddressReg>()? {
            if addr_reg.chip_rev != bm1387::ChipRev::Bm1387 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "Unexpected revision of chip {} (expected: {:?} received: {:?})",
                        self.chip_count,
                        addr_reg.chip_rev,
                        bm1387::ChipRev::Bm1387
                    ),
                ));
            }
            self.chip_count += 1;
        }
        // TODO remove once we have a dedicated heartbeat task
        self.send_voltage_ctrl_heart_beat()?;

        if self.chip_count >= MAX_CHIPS_ON_CHAIN {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Detected {} chips, expected less than 256 chips on 1 chain. Possibly a hardware issue?",
                    self.chip_count
                ),
            ));
        }
        if self.chip_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "No chips detected on the current chain.",
            ));
        }
        // Set all chips to be offline before address assignment. This is important so that each
        // chip after initially accepting the address will pass on further addresses down the chain
        let inactivate_from_chain_cmd = bm1387::InactivateFromChainCmd::new().pack();
        // make sure all chips receive inactivation request
        for _ in 0..3 {
            self.send_ctl_cmd(&inactivate_from_chain_cmd, false);
            thread::sleep(Duration::from_millis(INACTIVATE_FROM_CHAIN_DELAY_MS));
        }

        // TODO remove once we have a dedicated heartbeat task
        self.send_voltage_ctrl_heart_beat()?;

        // Assign address to each chip
        self.for_all_chips(|addr| {
            let cmd = bm1387::SetChipAddressCmd::new(addr);
            self.send_ctl_cmd(&cmd.pack(), false);
            Ok(())
        })?;

        // TODO remove once we have a dedicated heartbeat task
        self.send_voltage_ctrl_heart_beat()?;

        Ok(())
    }

    /// Helper method that applies a function to all detected chips on the chain
    fn for_all_chips<F, R>(&self, f: F) -> Result<R, io::Error>
    where
        F: Fn(u8) -> Result<R, io::Error>,
    {
        let mut result: Result<R, io::Error> =
            Err(io::Error::new(io::ErrorKind::Other, "no chips to iterate"));
        for addr in (0..self.chip_count * 4).step_by(4) {
            // the enumeration takes care that address always fits into 8 bits.
            // Therefore, we can truncate the bits here.
            result = Ok(f(addr as u8)?);
        }
        // Result of last iteration
        result
    }

    /// Loads PLL register with a starting value
    fn set_pll(&self) -> Result<(), io::Error> {
        self.for_all_chips(|addr| {
            let cmd = bm1387::SetConfigCmd::new(addr, false, bm1387::PLL_PARAM_REG, 0x21026800);
            self.send_ctl_cmd(&cmd.pack(), false);
            Ok(())
        })
    }

    /// TODO: consolidate hardcoded baudrate to 115200
    fn configure_hash_chain(&self) -> Result<(), io::Error> {
        let ctl_reg = bm1387::MiscCtrlReg {
            not_set_baud: true,
            inv_clock: true,
            baud_div: 26.into(),
            gate_block: true,
            mmen: true,
        };
        let cmd = bm1387::SetConfigCmd::new(0, true, bm1387::MISC_CONTROL_REG, ctl_reg.into());
        // wait until all commands have been sent
        self.send_ctl_cmd(&cmd.pack(), true);
        Ok(())
    }

    fn enable_ip_core(&self) {
        self.hash_chain_ios[0]
            .ctrl_reg
            .modify(|_, w| w.enable().bit(true));
    }

    fn disable_ip_core(&self) {
        self.hash_chain_ios[0]
            .ctrl_reg
            .modify(|_, w| w.enable().bit(false));
    }

    fn set_work_time(&self, work_time: u32) {
        self.hash_chain_ios[0]
            .work_time
            .write(|w| unsafe { w.bits(work_time) });
    }

    /// TODO make parametric and remove hardcoded baudrate constant
    fn set_baud(&self, _baud: u32) {
        self.hash_chain_ios[0]
            .baud_reg
            .write(|w| unsafe { w.bits(0x1b) });
    }

    fn set_midstate_count(&self) {
        self.hash_chain_ios[0]
            .ctrl_reg
            .modify(|_, w| unsafe { w.midstate_cnt().bits(self.midstate_count_bits) });
    }

    fn u256_as_u32_slice(src: &uint::U256) -> &[u32] {
        unsafe {
            core::slice::from_raw_parts(
                src.0.as_ptr() as *const u32,
                size_of::<uint::U256>() / size_of::<u32>(),
            )
        }
    }

    #[inline]
    /// Work ID's are generated with a step that corresponds to the number of configured midstates
    fn next_work_id(&mut self) -> u32 {
        let retval = self.work_id as u32;
        self.work_id += 1 << self.midstate_count_bits;
        retval
    }

    #[inline]
    /// TODO: implement error handling/make interface ready for ASYNC execution
    /// Writes single word into a TX fifo
    fn write_to_work_tx_fifo(&self, item: u32) {
        let hash_chain_io = self.hash_chain_ios[0];
        while hash_chain_io.stat_reg.read().work_tx_full().bit() {}
        hash_chain_io
            .work_tx_fifo
            .write(|w| unsafe { w.bits(item) });
    }

    #[inline]
    fn read_from_work_rx_fifo(&self) -> Result<u32, io::Error> {
        let hash_chain_io = self.hash_chain_ios[0];
        // TODO temporary workaround until we have asynchronous handling - wait 5 ms if the FIFO
        // is empty
        if hash_chain_io.stat_reg.read().work_rx_empty().bit() {
            thread::sleep(Duration::from_millis(5));
        }
        if hash_chain_io.stat_reg.read().work_rx_empty().bit() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Work RX fifo empty",
            ));
        }
        Ok(hash_chain_io.work_rx_fifo.read().bits())
    }

    #[inline]
    /// TODO get rid of busy waiting, prepare for non-blocking API
    fn write_to_cmd_tx_fifo(&self, item: u32) {
        let hash_chain_io = self.hash_chain_ios[0];
        while hash_chain_io.stat_reg.read().cmd_tx_full().bit() {}
        hash_chain_io.cmd_tx_fifo.write(|w| unsafe { w.bits(item) });
    }

    #[inline]
    fn read_from_cmd_rx_fifo(&self) -> Result<u32, io::Error> {
        let hash_chain_io = self.hash_chain_ios[0];
        // TODO temporary workaround until we have asynchronous handling - wait 5 ms if the FIFO
        // is empty
        if hash_chain_io.stat_reg.read().cmd_rx_empty().bit() {
            thread::sleep(Duration::from_millis(5));
        }
        if hash_chain_io.stat_reg.read().cmd_rx_empty().bit() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Command RX fifo empty, read has timedout",
            ));
        }
        Ok(hash_chain_io.cmd_rx_fifo.read().bits())
    }

    #[inline]
    /// Helper function that extracts work ID from the result ID
    fn get_work_id_from_result_id(&self, result_id: u32) -> u32 {
        ((result_id >> WORK_ID_OFFSET) >> self.midstate_count_bits)
    }

    #[inline]
    /// Extracts midstate index from the result ID
    fn get_midstate_idx_from_result_id(&self, result_id: u32) -> usize {
        ((result_id >> WORK_ID_OFFSET) & ((1u32 << self.midstate_count_bits) - 1)) as usize
    }

    #[inline]
    /// Extracts solution index from the result ID
    fn get_solution_idx_from_result_id(&self, result_id: u32) -> usize {
        (result_id & ((1u32 << WORK_ID_OFFSET) - 1)) as usize
    }

    /// Serializes command into 32-bit words and submits it to the command TX FIFO
    ///
    /// * `wait` - when true, wait until all commands are sent
    fn send_ctl_cmd(&self, cmd: &[u8], wait: bool) {
        // invariant required by the IP core
        assert_eq!(
            cmd.len() & 0x3,
            0,
            "Control command length not aligned to 4 byte boundary!"
        );
        for chunk in cmd.chunks(4) {
            self.write_to_cmd_tx_fifo(LittleEndian::read_u32(chunk));
        }
        // TODO busy waiting has to be replaced once asynchronous processing is in place
        if wait {
            while !self.hash_chain_ios[0].stat_reg.read().cmd_tx_empty().bit() {}
        }
    }

    /// Command responses are always 7 bytes long including checksum. Therefore, the reception
    /// has to be done in 2 steps with the following error handling:
    ///
    /// - A timeout when reading the first word is converted into an empty response.
    ///   The method propagates any error other than timeout
    /// - An error that occurs during reading the second word from the FIFO is propagated.
    fn recv_ctl_cmd_resp<T: PackedStructSlice>(&self) -> Result<Option<T>, io::Error> {
        let mut cmd_resp = [0u8; 8];

        // TODO: to be refactored once we have asynchronous handling in place
        // fetch command response from IP core's fifo
        match self.read_from_cmd_rx_fifo() {
            Err(e) => if e.kind() == io::ErrorKind::TimedOut {
                return Ok(None);
            } else {
                return Err(e);
            },
            Ok(resp_word) => LittleEndian::write_u32(&mut cmd_resp[..4], resp_word),
        }
        // All errors from reading the second word are propagated
        let resp_word = self.read_from_cmd_rx_fifo()?;
        LittleEndian::write_u32(&mut cmd_resp[4..], resp_word);

        // build the response instance - drop the extra byte due to FIFO being 32-bit word based
        // and drop the checksum
        // TODO: optionally verify the checksum (use debug_assert?)
        T::unpack_from_slice(&cmd_resp[..6])
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "Control command unpacking error! {:?} {:#04x?}",
                        e, cmd_resp
                    ),
                )
            }).map(|resp| Some(resp))
    }
}

impl<'a, 'b> super::HardwareCtl for HChainCtl<'a, 'b> {
    fn send_work(&mut self, work: &super::MiningWork) -> Result<u32, io::Error> {
        let work_id = self.next_work_id();

        // TODO remove once we have a dedicated heartbeat task
        self.send_voltage_ctrl_heart_beat()?;

        self.write_to_work_tx_fifo(work_id);
        self.write_to_work_tx_fifo(work.nbits);
        self.write_to_work_tx_fifo(work.ntime);
        self.write_to_work_tx_fifo(work.merkel_root_lsw);

        for midstate in work.midstates.iter() {
            let midstate = HChainCtl::u256_as_u32_slice(&midstate);
            // Chip expects the midstate in reverse word order
            for midstate_word in midstate.iter().rev() {
                self.write_to_work_tx_fifo(*midstate_word);
            }
        }
        Ok(work_id)
    }

    fn recv_work_result(&mut self) -> Result<Option<super::MiningWorkResult>, io::Error> {
        // TODO remove once we have a dedicated heartbeat task
        self.send_voltage_ctrl_heart_beat()?;

        let nonce; // = self.read_from_work_rx_fifo()?;
                   // TODO: to be refactored once we have asynchronous handling in place
                   // fetch command response from IP core's fifo
        match self.read_from_work_rx_fifo() {
            Err(e) => if e.kind() == io::ErrorKind::TimedOut {
                return Ok(None);
            } else {
                return Err(e);
            },
            Ok(resp_word) => nonce = resp_word,
        }

        let word2 = self.read_from_work_rx_fifo()?;

        let result = super::MiningWorkResult {
            nonce,
            // this hardware doesn't do any nTime rolling, keep it @ None
            ntime: None,
            midstate_idx: self.get_midstate_idx_from_result_id(word2),
            // leave the result ID as-is so that we can extract solution index etc later.
            result_id: word2 & 0xffffffu32,
        };

        Ok(Some(result))
    }

    fn get_work_id_from_result(&self, result: &super::MiningWorkResult) -> u32 {
        self.get_work_id_from_result_id(result.result_id)
    }

    fn get_chip_count(&self) -> usize {
        self.chip_count
    }
}

#[cfg(test)]
mod test {
    use super::*;
    //    use std::sync::{Once, ONCE_INIT};
    //
    //    static H_CHAIN_CTL_INIT: Once = ONCE_INIT;
    //    static mut H_CHAIN_CTL: HChainCtl = HChainCtl {
    //
    //    };
    //
    //    fn get_ctl() -> Result<HChainCtl, io::Error>  {
    //        H_CHAIN_CTL.call_once(|| {
    //            let h_chain_ctl = HChainCtl::new();
    //        });
    //        h_chain_ctl
    //    }

    #[test]
    fn test_hchain_ctl_instance() {
        let gpio_mgr = gpio::ControlPinManager::new();
        let mut voltage_ctrl_backend = power::VoltageCtrlI2cBlockingBackend::<I2cdev>::new(0);
        let h_chain_ctl = HChainCtl::new(
            &gpio_mgr,
            &mut voltage_ctrl_backend,
            8,
            &hchainio0::ctrl_reg::MIDSTATE_CNTW::ONE,
        );
        match h_chain_ctl {
            Ok(_) => assert!(true),
            Err(e) => assert!(false, "Failed to instantiate hash chain, error: {}", e),
        }
    }

    #[test]
    fn test_hchain_ctl_init() {
        let gpio_mgr = gpio::ControlPinManager::new();
        let mut voltage_ctrl_backend = power::VoltageCtrlI2cBlockingBackend::<I2cdev>::new(0);
        let h_chain_ctl = HChainCtl::new(
            &gpio_mgr,
            &mut voltage_ctrl_backend,
            8,
            &hchainio0::ctrl_reg::MIDSTATE_CNTW::ONE,
        ).expect("Failed to create hash board instance");

        assert!(
            h_chain_ctl.ip_core_init().is_ok(),
            "Failed to initialize IP core"
        );

        // verify sane register values
        assert_eq!(
            h_chain_ctl.hash_chain_ios[0].work_time.read().bits(),
            50000,
            "Unexpected work time value"
        );
        assert_eq!(
            h_chain_ctl.hash_chain_ios[0].baud_reg.read().bits(),
            0x1b,
            "Unexpected baudrate register value"
        );
        assert_eq!(
            h_chain_ctl.hash_chain_ios[0].stat_reg.read().bits(),
            0x855,
            "Unexpected status register value"
        );
        assert_eq!(
            h_chain_ctl.hash_chain_ios[0].ctrl_reg.read().midstate_cnt(),
            hchainio0::ctrl_reg::MIDSTATE_CNTR::ONE,
            "Unexpected midstate count"
        );
    }

    /// This test verifies correct parsing of work result for all multi-midstate configurations.
    /// The result_word represents the second word of the work result (after nonce) provided by the
    /// FPGA IP core
    #[test]
    fn test_get_result_word_attributes() {
        let result_word = 0x00123502;
        struct ExpectedResultData {
            work_id: u32,
            midstate_idx: usize,
            solution_idx: usize,
            midstate_count: hchainio0::ctrl_reg::MIDSTATE_CNTW,
        };
        let expected_result_data = [
            ExpectedResultData {
                work_id: 0x1235,
                midstate_idx: 0,
                solution_idx: 2,
                midstate_count: hchainio0::ctrl_reg::MIDSTATE_CNTW::ONE,
            },
            ExpectedResultData {
                work_id: 0x1235 >> 1,
                midstate_idx: 1,
                solution_idx: 2,
                midstate_count: hchainio0::ctrl_reg::MIDSTATE_CNTW::TWO,
            },
            ExpectedResultData {
                work_id: 0x1235 >> 2,
                midstate_idx: 1,
                solution_idx: 2,
                midstate_count: hchainio0::ctrl_reg::MIDSTATE_CNTW::FOUR,
            },
        ];
        for (i, expected_result_data) in expected_result_data.iter().enumerate() {
            // The midstate configuration (ctrl_reg::MIDSTATE_CNTW) doesn't implement a debug
            // trait. Therefore, we extract only those parts that can be easily displayed when a
            // test failed.
            let expected_data = (
                expected_result_data.work_id,
                expected_result_data.midstate_idx,
                expected_result_data.solution_idx,
            );
            let gpio_mgr = gpio::ControlPinManager::new();
            let mut voltage_ctrl_backend = power::VoltageCtrlI2cBlockingBackend::<I2cdev>::new(0);
            let h_chain_ctl = HChainCtl::new(
                &gpio_mgr,
                &mut voltage_ctrl_backend,
                8,
                &expected_result_data.midstate_count,
            ).unwrap();

            assert_eq!(
                h_chain_ctl.get_work_id_from_result_id(result_word),
                expected_result_data.work_id,
                "Invalid work ID, iteration: {}, test data: {:#06x?}",
                i,
                expected_data
            );
            assert_eq!(
                h_chain_ctl.get_midstate_idx_from_result_id(result_word),
                expected_result_data.midstate_idx,
                "Invalid midstate index, iteration: {}, test data: {:#06x?}",
                i,
                expected_data
            );
            assert_eq!(
                h_chain_ctl.get_solution_idx_from_result_id(result_word),
                expected_result_data.solution_idx,
                "Invalid solution index, iteration: {}, test data: {:#06x?}",
                i,
                expected_data
            );
        }
    }

}