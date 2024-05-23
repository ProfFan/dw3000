#![allow(unused_imports)]

use core::convert::TryInto;

use byte::BytesExt as _;
use embedded_hal::spi;
use fixed::traits::LossyInto;

#[cfg(feature = "defmt")]
use defmt::Format;

use super::{AutoDoubleBufferReceiving, ReceiveTime, Receiving};
use crate::{
    configs::{BitRate, SfdSequence},
    time::Instant,
    Config, Error, FastCommand, Ready, DW3000,
};

use smoltcp::wire::Ieee802154Frame;

/// An incoming message
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(Format))]
pub struct Message<'l> {
    /// The time the message was received
    ///
    /// This time is based on the local system time, as defined in the SYS_TIME
    /// register.
    pub rx_time: Instant,

    /// The MAC frame
    pub frame: Ieee802154Frame<&'l [u8]>,
}

/// A struct representing the quality of the received message.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RxQuality {
    /// The confidence that there was Line Of Sight between the sender and the
    /// receiver.
    ///
    /// - 0 means it's very unlikely there was LOS.
    /// - 1 means it's very likely there was LOS.
    ///
    /// The number doesn't give a guarantee, but an indication.
    /// It is based on the
    /// APS006_Part-3-DW3000-Diagnostics-for-NLOS-Channels-v1.1 document.
    pub los_confidence_level: f32,
    /// The radio signal strength indicator in dBm.
    ///
    /// The value is an estimation that is quite accurate up to -85 dBm.
    /// Above -85 dBm, the estimation underestimates the actual value.
    pub rssi: f32,
}

impl<SPI, RECEIVING> DW3000<SPI, RECEIVING>
where
    SPI: spi::SpiDevice<u8>,
    RECEIVING: Receiving,
{
    /// Returns the RX state of the DW3000
    pub fn rx_state(&mut self) -> Result<u8, Error<SPI>> {
        Ok(self.ll.sys_state().read()?.rx_state())
    }

    pub(super) fn start_receiving(
        &mut self,
        recv_time: ReceiveTime,
        config: Config,
    ) -> Result<(), Error<SPI>> {
        if config.frame_filtering {
            self.ll.sys_cfg().modify(
                |_, w| w.ffen(0b1), // enable frame filtering
            )?;
            self.ll.ff_cfg().modify(
                |_, w| {
                    w.ffab(0b1) // receive beacon frames
                        .ffad(0b1) // receive data frames
                        .ffaa(0b1) // receive acknowledgement frames
                        .ffam(0b1)
                }, // receive MAC command frames
            )?;
        } else {
            self.ll.sys_cfg().modify(|_, w| w.ffen(0b0))?; // disable frame filtering
        }

        match recv_time {
            ReceiveTime::Delayed(time) => {
                // Panic if the time is not rounded to top 31 bits
                //
                // NOTE: DW3000's DX_TIME register is 32 bits wide, but only the top 31 bits are used.
                // The last bit is ignored per the user manual!!!
                if time.value() % (1 << 9) != 0 {
                    panic!("Time must be rounded to top 31 bits!");
                }

                // Put the time into the delay register
                // By setting this register, the chip knows to delay before transmitting
                self.ll
                    .dx_time()
                    .modify(|_, w| // 32-bits value of the most significant bits
                    w.value( (time.value() >> 8) as u32 ))?;
                self.fast_cmd(FastCommand::CMD_DRX)?;
            }
            ReceiveTime::Now => self.fast_cmd(FastCommand::CMD_RX)?,
        }

        Ok(())
    }

    /// Wait for receive operation to finish
    ///
    /// This method returns an `nb::Result` to indicate whether the transmission
    /// has finished, or whether it is still ongoing. You can use this to busily
    /// wait for the transmission to finish, for example using `nb`'s `block!`
    /// macro, or you can use it in tandem with [`DW3000::enable_rx_interrupts`]
    /// and the DW3000 IRQ output to wait in a more energy-efficient manner.
    ///
    /// Handling the DW3000's IRQ output line is out of the scope of this
    /// driver, but please note that if you're using the DWM1001 module or
    /// DWM1001-Dev board, that the `dwm1001` crate has explicit support for
    /// this.
    pub fn r_wait<'b>(&mut self, buffer: &'b mut [u8]) -> nb::Result<Message<'b>, Error<SPI>> {
        // ATTENTION:
        // If you're changing anything about which SYS_STATUS flags are being
        // checked in this method, also make sure to update `enable_interrupts`.
        let sys_status = self
            .ll()
            .sys_status()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        // Is a frame ready?
        if sys_status.rxfcg() == 0b0 {
            // No frame ready. Check for errors.
            if sys_status.rxfce() == 0b1 {
                return Err(nb::Error::Other(Error::Fcs));
            }
            if sys_status.rxphe() == 0b1 {
                return Err(nb::Error::Other(Error::Phy));
            }
            if sys_status.rxfsl() == 0b1 {
                return Err(nb::Error::Other(Error::ReedSolomon));
            }
            if sys_status.rxsto() == 0b1 {
                return Err(nb::Error::Other(Error::SfdTimeout));
            }
            if sys_status.arfe() == 0b1 {
                return Err(nb::Error::Other(Error::FrameFilteringRejection));
            }
            if sys_status.rxfto() == 0b1 {
                return Err(nb::Error::Other(Error::FrameWaitTimeout));
            }
            if sys_status.rxovrr() == 0b1 {
                return Err(nb::Error::Other(Error::Overrun));
            }
            if sys_status.rxpto() == 0b1 {
                return Err(nb::Error::Other(Error::PreambleDetectionTimeout));
            }

            // Some error flags that sound like valid errors aren't checked here,
            // because experience has shown that they seem to occur spuriously
            // without preventing a good frame from being received. Those are:
            // - LDEERR: Leading Edge Detection Processing Error
            // - RXPREJ: Receiver Preamble Rejection

            // No errors detected. That must mean the frame is just not ready yet.
            return Err(nb::Error::WouldBlock);
        }

        // Frame is ready. Continue.

        // Wait until LDE processing is done. Before this is finished, the RX
        // time stamp is not available.
        let rx_time = self
            .ll()
            .rx_time()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?
            .rx_stamp();

        // `rx_time` comes directly from the register, which should always
        // contain a 40-bit timestamp. Unless the hardware or its documentation
        // are buggy, the following should never panic.
        let rx_time = Instant::new(rx_time).unwrap();

        //  Reset status bits. This is not strictly necessary, but it helps, if
        // you have to inspect SYS_STATUS manually during debugging.
        self.ll()
            .sys_status()
            .write(|w| {
                w.rxprd(0b1) // Receiver Preamble Detected
                    .rxsfdd(0b1) // Receiver SFD Detected
                    .ciadone(0b1) // LDE Processing Done
                    .rxphd(0b1) // Receiver PHY Header Detected
                    .rxphe(0b1) // Receiver PHY Header Error
                    .rxfr(0b1) // Receiver Data Frame Ready
                    .rxfcg(0b1) // Receiver FCS Good
                    .rxfce(0b1) // Receiver FCS Error
                    .rxfsl(0b1) // Receiver Reed Solomon Frame Sync Loss
                    .rxfto(0b1) // Receiver Frame Wait Timeout
                    .ciaerr(0b1) // Leading Edge Detection Processing Error
                    .rxovrr(0b1) // Receiver Overrun
                    .rxpto(0b1) // Preamble Detection Timeout
                    .rxsto(0b1) // Receiver SFD Timeout
                    .rxprej(0b1) // Receiver Preamble Rejection
            })
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        // Read received frame
        let rx_finfo = self
            .ll()
            .rx_finfo()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;
        let rx_buffer = self
            .ll()
            .rx_buffer_0()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        let len = rx_finfo.rxflen() as usize;

        if buffer.len() < len {
            return Err(nb::Error::Other(Error::BufferTooSmall {
                required_len: len,
            }));
        }

        buffer[..len].copy_from_slice(&rx_buffer.data()[..len]);

        let buffer = &buffer[..len];

        self.state.mark_finished();

        let frame = Ieee802154Frame::new_checked(buffer).unwrap();

        Ok(Message { rx_time, frame })
    }

    /// Wait for receive operation to finish
    ///
    /// This method returns an `nb::Result` to indicate whether the transmission
    /// has finished, or whether it is still ongoing. You can use this to busily
    /// wait for the transmission to finish, for example using `nb`'s `block!`
    /// macro, or you can use it in tandem with [`DW3000::enable_rx_interrupts`]
    /// and the DW3000 IRQ output to wait in a more energy-efficient manner.
    ///
    /// Handling the DW3000's IRQ output line is out of the scope of this
    /// driver, but please note that if you're using the DWM1001 module or
    /// DWM1001-Dev board, that the `dwm1001` crate has explicit support for
    /// this.
    pub fn r_wait_buf(&mut self, buffer: &mut [u8]) -> nb::Result<(usize, Instant), Error<SPI>> {
        // ATTENTION:
        // If you're changing anything about which SYS_STATUS flags are being
        // checked in this method, also make sure to update `enable_interrupts`.
        let sys_status = self
            .ll()
            .sys_status()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        // Is a frame ready?
        if sys_status.rxfcg() == 0b0 {
            // No frame ready. Check for errors.
            if sys_status.rxfce() == 0b1 {
                return Err(nb::Error::Other(Error::Fcs));
            }
            if sys_status.rxphe() == 0b1 {
                return Err(nb::Error::Other(Error::Phy));
            }
            if sys_status.rxfsl() == 0b1 {
                return Err(nb::Error::Other(Error::ReedSolomon));
            }
            if sys_status.rxsto() == 0b1 {
                return Err(nb::Error::Other(Error::SfdTimeout));
            }
            if sys_status.arfe() == 0b1 {
                return Err(nb::Error::Other(Error::FrameFilteringRejection));
            }
            if sys_status.rxfto() == 0b1 {
                return Err(nb::Error::Other(Error::FrameWaitTimeout));
            }
            if sys_status.rxovrr() == 0b1 {
                return Err(nb::Error::Other(Error::Overrun));
            }
            if sys_status.rxpto() == 0b1 {
                return Err(nb::Error::Other(Error::PreambleDetectionTimeout));
            }

            // Some error flags that sound like valid errors aren't checked here,
            // because experience has shown that they seem to occur spuriously
            // without preventing a good frame from being received. Those are:
            // - LDEERR: Leading Edge Detection Processing Error
            // - RXPREJ: Receiver Preamble Rejection

            // No errors detected. That must mean the frame is just not ready yet.
            return Err(nb::Error::WouldBlock);
        }

        // Frame is ready. Continue.

        // Wait until LDE processing is done. Before this is finished, the RX
        // time stamp is not available.
        let rx_time = self
            .ll()
            .rx_time()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?
            .rx_stamp();

        // `rx_time` comes directly from the register, which should always
        // contain a 40-bit timestamp. Unless the hardware or its documentation
        // are buggy, the following should never panic.
        let rx_time = Instant::new(rx_time).unwrap();

        //  Reset status bits. This is not strictly necessary, but it helps, if
        // you have to inspect SYS_STATUS manually during debugging.
        self.ll()
            .sys_status()
            .write(|w| {
                w.rxprd(0b1) // Receiver Preamble Detected
                    .rxsfdd(0b1) // Receiver SFD Detected
                    .ciadone(0b1) // LDE Processing Done
                    .rxphd(0b1) // Receiver PHY Header Detected
                    .rxphe(0b1) // Receiver PHY Header Error
                    .rxfr(0b1) // Receiver Data Frame Ready
                    .rxfcg(0b1) // Receiver FCS Good
                    .rxfce(0b1) // Receiver FCS Error
                    .rxfsl(0b1) // Receiver Reed Solomon Frame Sync Loss
                    .rxfto(0b1) // Receiver Frame Wait Timeout
                    .ciaerr(0b1) // Leading Edge Detection Processing Error
                    .rxovrr(0b1) // Receiver Overrun
                    .rxpto(0b1) // Preamble Detection Timeout
                    .rxsto(0b1) // Receiver SFD Timeout
                    .rxprej(0b1) // Receiver Preamble Rejection
            })
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        // Read received frame
        let rx_finfo = self
            .ll()
            .rx_finfo()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;
        let rx_buffer = self
            .ll()
            .rx_buffer_0()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        let len = rx_finfo.rxflen() as usize;

        if buffer.len() < len {
            return Err(nb::Error::Other(Error::BufferTooSmall {
                required_len: len,
            }));
        }

        buffer[..len].copy_from_slice(&rx_buffer.data()[..len]);

        self.state.mark_finished();

        Ok((len, rx_time))
    }

    #[allow(clippy::type_complexity)]
    /// Finishes receiving and returns to the `Ready` state
    ///
    /// If the receive operation has finished, as indicated by `wait`, this is a
    /// no-op. If the receive operation is still ongoing, it will be aborted.
    pub fn finish_receiving(mut self) -> Result<DW3000<SPI, Ready>, (Self, Error<SPI>)> {
        // TO DO : if we are not in state 3 (IDLE), we need to have a reset of the module (with a new initialisation)
        // BECAUSE : using force_idle (fast command 0) is not puting the pll back to stable !!!

        if !self.state.is_finished() {
            match self.force_idle() {
                Ok(()) => (),
                Err(error) => return Err((self, error)),
            }
        }

        Ok(DW3000 {
            ll: self.ll,
            seq: self.seq,
            state: Ready,
        })
    }
}
