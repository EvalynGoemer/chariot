use std::io::{self, Write};

use indicatif::ProgressBar;

pub struct ProgressBarWriter<'a> {
    buffer: Vec<u8>,
    bar: &'a ProgressBar,
}

impl<'a> ProgressBarWriter<'a> {
    pub fn init(bar: &'a ProgressBar) -> Self {
        Self { bar, buffer: Vec::new() }
    }
}

impl<'a> Write for ProgressBarWriter<'a> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let count = buffer.len();

        for b in &buffer[..count] {
            if b.to_ascii_lowercase() as char == '\n' {
                self.bar.set_message(String::from_iter(self.buffer.iter().map(|b| *b as char)));
                self.buffer.clear();
                continue;
            }

            self.buffer.push(*b);
        }

        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
