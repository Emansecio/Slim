use std::io;

use super::repository::{DurableAppendValidator, DurableRepo};
use super::schema_v2::{DurableRecord, DurableSessionHeader};

pub struct MemoryRepo {
    header: DurableSessionHeader,
    records: Vec<DurableRecord>,
    validator: DurableAppendValidator,
}

impl MemoryRepo {
    pub fn new(header: DurableSessionHeader) -> Self {
        Self {
            header,
            records: Vec::new(),
            validator: DurableAppendValidator::empty(),
        }
    }
}

impl DurableRepo for MemoryRepo {
    fn header(&self) -> &DurableSessionHeader {
        &self.header
    }

    fn records(&self) -> &[DurableRecord] {
        &self.records
    }

    fn append(&mut self, record: DurableRecord) -> io::Result<()> {
        let prepared = self.validator.prepare(&self.records, &record)?;
        self.records.push(record);
        self.validator.commit(prepared);
        Ok(())
    }

    fn append_batch(&mut self, records: Vec<DurableRecord>) -> io::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let prepared = self.validator.prepare_batch(&self.records, &records)?;
        self.records.extend(records);
        self.validator.commit(prepared);
        Ok(())
    }
}
