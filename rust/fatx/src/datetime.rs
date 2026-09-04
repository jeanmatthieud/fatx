use crate::variant::Variant;

pub struct Date {
    year: u16,
    /// 1 = January
    month: u8,
    day: u8,
}

impl Date {
    pub fn from_fatx_encoding(encoded: u16, variant: Variant) -> Self {
        Self {
            year: (((encoded >> 9) & 0x7f) + variant.epoch()),
            month: ((encoded >> 5) & 0xf) as u8,
            day: (encoded & 0x1f) as u8,
        }
    }

    /// Pack this date into its on-disk form.
    ///
    /// The year is stored as an offset from the filesystem's epoch, in seven
    /// bits, so a date outside that window wraps rather than being rejected.
    pub fn to_fatx_encoding(&self, variant: Variant) -> u16 {
        let year = self.year.wrapping_sub(variant.epoch()) & 0x7f;
        (self.day as u16 & 0x1f) | ((self.month as u16 & 0xf) << 5) | (year << 9)
    }

    pub fn year(&self) -> u16 {
        self.year
    }
    /// Returns the month, where 1 = January
    pub fn month(&self) -> u8 {
        self.month
    }
    pub fn day(&self) -> u8 {
        self.day
    }
}

pub struct Time {
    hour: u8,
    minute: u8,
    second: u8,
}

impl Time {
    /// The Xbox 360 uses the standard FAT field widths, 5 bits of hour and 6
    /// of minute. The original Xbox uses 4 and 5, which cannot represent an
    /// hour past 15 or a minute past 31.
    pub fn from_fatx_encoding(encoded: u16, variant: Variant) -> Self {
        let (hour_mask, minute_mask) = Self::field_masks(variant);
        Self {
            hour: ((encoded >> 11) & hour_mask) as u8,
            minute: ((encoded >> 5) & minute_mask) as u8,
            second: ((encoded & 0x1f) * 2) as u8,
        }
    }

    /// Pack this time into its on-disk form.
    ///
    /// Seconds are stored with a two second resolution, so an odd second is
    /// rounded down. On the original Xbox the hour and minute fields are too
    /// narrow to hold every value; they are masked, exactly as libfatx does.
    pub fn to_fatx_encoding(&self, variant: Variant) -> u16 {
        let (hour_mask, minute_mask) = Self::field_masks(variant);
        ((self.hour as u16 & hour_mask) << 11)
            | ((self.minute as u16 & minute_mask) << 5)
            | ((self.second as u16 / 2) & 0x1f)
    }

    fn field_masks(variant: Variant) -> (u16, u16) {
        match variant {
            Variant::X360 => (0x1f, 0x3f),
            _ => (0xf, 0x1f),
        }
    }

    pub fn hour(&self) -> u8 {
        self.hour
    }
    pub fn minute(&self) -> u8 {
        self.minute
    }
    pub fn second(&self) -> u8 {
        self.second
    }
}

pub struct DateTime {
    date: Date,
    time: Time,
}

impl DateTime {
    pub fn from_fatx_encoding(date_encoded: u16, time_encoded: u16, variant: Variant) -> Self {
        Self {
            date: Date::from_fatx_encoding(date_encoded, variant),
            time: Time::from_fatx_encoding(time_encoded, variant),
        }
    }

    /// Build a timestamp from broken-down local time, where 1 = January.
    pub fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Self {
        Self {
            date: Date { year, month, day },
            time: Time {
                hour,
                minute,
                second,
            },
        }
    }

    /// The current local time.
    ///
    /// FATX timestamps are local wall-clock time with no time zone of their
    /// own, which is what the consoles themselves write.
    pub fn now() -> Self {
        use chrono::{Datelike, Local, Timelike};

        let now = Local::now();
        Self::new(
            now.year() as u16,
            now.month() as u8,
            now.day() as u8,
            now.hour() as u8,
            now.minute() as u8,
            now.second() as u8,
        )
    }

    /// Pack this timestamp into its on-disk form, as a (date, time) pair.
    pub fn to_fatx_encoding(&self, variant: Variant) -> (u16, u16) {
        (
            self.date.to_fatx_encoding(variant),
            self.time.to_fatx_encoding(variant),
        )
    }

    pub fn year(&self) -> u16 {
        self.date.year
    }
    /// Returns the month, where 1 = January
    pub fn month(&self) -> u8 {
        self.date.month
    }
    pub fn day(&self) -> u8 {
        self.date.day
    }
    pub fn hour(&self) -> u8 {
        self.time.hour
    }
    pub fn minute(&self) -> u8 {
        self.time.minute
    }
    pub fn second(&self) -> u8 {
        self.time.second
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An hour and a minute that do not fit the original Xbox's narrower
    /// fields, so a regression to those widths cannot pass unnoticed.
    const X360_STAMP: (u16, u8, u8, u8, u8, u8) = (2026, 8, 11, 23, 50, 30);

    fn round_trip(stamp: (u16, u8, u8, u8, u8, u8), variant: Variant) -> DateTime {
        let dt = DateTime::new(stamp.0, stamp.1, stamp.2, stamp.3, stamp.4, stamp.5);
        let (date, time) = dt.to_fatx_encoding(variant);
        DateTime::from_fatx_encoding(date, time, variant)
    }

    #[test]
    fn x360_timestamp_round_trips() {
        let dt = round_trip(X360_STAMP, Variant::X360);
        assert_eq!(
            (
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second()
            ),
            X360_STAMP
        );
    }

    #[test]
    fn xbox_timestamp_round_trips() {
        let stamp = (2003, 12, 25, 13, 24, 44);
        let dt = round_trip(stamp, Variant::Xbox);
        assert_eq!(
            (
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second()
            ),
            stamp
        );
    }

    #[test]
    fn the_two_variants_encode_the_same_stamp_differently() {
        let dt = DateTime::new(2026, 8, 11, 23, 50, 30);
        assert_ne!(
            dt.to_fatx_encoding(Variant::Xbox),
            dt.to_fatx_encoding(Variant::X360)
        );
    }
}
