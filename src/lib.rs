//! A rolling file appender with customizable rolling conditions.
//! Includes built-in support for rolling conditions on date/time
//! (daily, hourly, every minute) and/or size.
//!
//! Follows a Debian-style naming convention for logfiles,
//! using basename, basename.1, ..., basename.N where N is
//! the maximum number of allowed historical logfiles.
//!
//! This is useful to combine with the tracing crate and
//! tracing_appender::non_blocking::NonBlocking -- use it
//! as an alternative to tracing_appender::rolling::RollingFileAppender.
//!
//! # Examples
//!
//! ```rust
//! # fn docs() {
//! # use tracing_rolling_file::*;
//! let file_appender = RollingFileAppenderBase::new(
//!     "/var/log/myprogram",
//!     RollingConditionBase::new().daily(),
//!     9
//! ).unwrap();
//! # }
//! ```
#![deny(warnings)]

use chrono::prelude::*;
use std::{
    convert::TryFrom,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::Path,
};

/// Determines when a file should be "rolled over".
pub trait RollingCondition {
    /// Determine and return whether or not the file should be rolled over.
    fn should_rollover(&mut self, now: &DateTime<Local>, current_filesize: u64) -> bool;
}

/// Determines how often a file should be rolled over
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RollingFrequency {
    EveryDay,
    EveryHour,
    EveryMinute,
    EveryNMinute(u32),
}

impl RollingFrequency {
    /// Calculates a datetime that will be different if data should be in
    /// different files.
    pub fn equivalent_datetime(&self, dt: &DateTime<Local>) -> DateTime<Local> {
        match self {
            RollingFrequency::EveryDay => Local
                .with_ymd_and_hms(dt.year(), dt.month(), dt.day(), 0, 0, 0)
                .unwrap(),
            RollingFrequency::EveryHour => Local
                .with_ymd_and_hms(dt.year(), dt.month(), dt.day(), dt.hour(), 0, 0)
                .unwrap(),
            RollingFrequency::EveryMinute => Local
                .with_ymd_and_hms(dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute(), 0)
                .unwrap(),
            RollingFrequency::EveryNMinute(m) => Local
                .with_ymd_and_hms(dt.year(), dt.month(), dt.day(), dt.hour(), (dt.minute() / m) * m, 0)
                .unwrap(),
        }
    }
}

/// Writes data to a file, and "rolls over" to preserve older data in
/// a separate set of files. Old files have a Debian-style naming scheme
/// where we have base_filename, base_filename.1, ..., base_filename.N
/// where N is the maximum number of rollover files to keep.
#[derive(Debug)]
pub struct RollingFileAppender<RC>
where
    RC: RollingCondition,
{
    condition: RC,
    filename: String,
    max_filecount: usize,
    current_filesize: u64,
    writer_opt: Option<BufWriter<File>>,
}

impl<RC> RollingFileAppender<RC>
where
    RC: RollingCondition,
{
    /// Creates a new rolling file appender with the given condition.
    /// The filename parent path must already exist.
    pub fn new(filename: impl AsRef<Path>, condition: RC, max_filecount: usize) -> io::Result<RollingFileAppender<RC>> {
        let filename = filename.as_ref().to_str().unwrap().to_string();
        let mut appender = RollingFileAppender {
            condition,
            filename,
            max_filecount,
            current_filesize: 0,
            writer_opt: None,
        };
        // Fail if we can't open the file initially...
        appender.open_writer_if_needed()?;
        Ok(appender)
    }

    /// Determines the final filename, where n==0 indicates the current file
    fn filename_for(&self, n: usize) -> String {
        let f = self.filename.clone();
        if n > 0 {
            format!("{}.{}", f, n)
        } else {
            f
        }
    }

    /// Rotates old files to make room for a new one.
    /// This may result in the deletion of the oldest file
    fn rotate_files(&mut self) -> io::Result<()> {
        // ignore any failure removing the oldest file (may not exist)
        let _ = fs::remove_file(self.filename_for(self.max_filecount.max(1)));
        let mut r = Ok(());
        for i in (0..self.max_filecount.max(1)).rev() {
            let rotate_from = self.filename_for(i);
            let rotate_to = self.filename_for(i + 1);
            if let Err(e) = fs::rename(&rotate_from, &rotate_to).or_else(|e| match e.kind() {
                io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            }) {
                // capture the error, but continue the loop,
                // to maximize ability to rename everything
                r = Err(e);
            }
        }
        r
    }

    /// Forces a rollover to happen immediately.
    pub fn rollover(&mut self) -> io::Result<()> {
        // Before closing, make sure all data is flushed successfully.
        self.flush()?;
        // We must close the current file before rotating files
        self.writer_opt.take();
        self.current_filesize = 0;
        self.rotate_files()?;
        self.open_writer_if_needed()
    }

    /// Opens a writer for the current file.
    fn open_writer_if_needed(&mut self) -> io::Result<()> {
        if self.writer_opt.is_none() {
            let p = self.filename_for(0);
            self.writer_opt = Some(BufWriter::new(OpenOptions::new().append(true).create(true).open(&p)?));
            self.current_filesize = fs::metadata(&p).map_or(0, |m| m.len());
        }
        Ok(())
    }

    /// Writes data using the given datetime to calculate the rolling condition
    pub fn write_with_datetime(&mut self, buf: &[u8], now: &DateTime<Local>) -> io::Result<usize> {
        if self.condition.should_rollover(now, self.current_filesize) {
            if let Err(e) = self.rollover() {
                // If we can't rollover, just try to continue writing anyway
                // (better than missing data).
                // This will likely used to implement logging, so
                // avoid using log::warn and log to stderr directly
                eprintln!("WARNING: Failed to rotate logfile {}: {}", self.filename, e);
            }
        }
        self.open_writer_if_needed()?;
        if let Some(writer) = self.writer_opt.as_mut() {
            let buf_len = buf.len();
            writer.write_all(buf).map(|_| {
                self.current_filesize += u64::try_from(buf_len).unwrap_or(u64::MAX);
                buf_len
            })
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "unexpected condition: writer is missing",
            ))
        }
    }
}

impl<RC> io::Write for RollingFileAppender<RC>
where
    RC: RollingCondition,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let now = Local::now();
        self.write_with_datetime(buf, &now)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(writer) = self.writer_opt.as_mut() {
            writer.flush()?;
        }
        Ok(())
    }
}

pub mod base;
pub use base::*;
