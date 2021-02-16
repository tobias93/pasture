use std::io::{Cursor, Read, Seek, SeekFrom};

use anyhow::{anyhow, Result};
use byteorder::{LittleEndian, NativeEndian, ReadBytesExt, WriteBytesExt};
use las_rs::{point::Format, Header};
use las_rs::{raw, Builder, Vlr};
use laz::{
    las::laszip::{LASZIP_RECORD_ID, LASZIP_USER_ID},
    LasZipDecompressor,
};
use pasture_core::{
    containers::InterleavedPointView,
    containers::{InterleavedVecPointStorage, PointBuffer, PointBufferWriteable},
    layout::attributes,
    layout::conversion::get_converter_for_attributes,
    layout::{conversion::AttributeConversionFn, PointLayout},
    meta::Metadata,
    nalgebra::Vector3,
    util::view_raw_bytes,
};

use super::{
    point_layout_from_las_point_format, BitAttributes, BitAttributesExtended, BitAttributesRegular,
    LASMetadata,
};
use crate::base::{PointReader, SeekToPoint};

/// Is the given VLR the LASzip VLR? Function taken from the `las` crate because it is not exported there
fn is_laszip_vlr(vlr: &Vlr) -> bool {
    if &vlr.user_id == LASZIP_USER_ID && vlr.record_id == LASZIP_RECORD_ID {
        true
    } else {
        false
    }
}

fn map_laz_err(laz_err: laz::LasZipError) -> anyhow::Error {
    anyhow!("LasZip error: {}", laz_err.to_string())
}

pub(crate) trait LASReaderBase {
    /// Returns the remaining number of points in the underyling `LASReaderBase`
    fn remaining_points(&self) -> usize;
}

pub(crate) struct RawLASReader<T: Read + Seek> {
    reader: T,
    metadata: LASMetadata,
    layout: PointLayout,
    current_point_index: usize,
    point_offsets: Vector3<f64>,
    point_scales: Vector3<f64>,
    offset_to_first_point_in_file: u64,
    size_of_point_in_file: u64,
    //TODO Add an option to not convert the position fields into world space
}

impl<T: Read + Seek> RawLASReader<T> {
    pub fn from_read(mut read: T) -> Result<Self> {
        let raw_header = raw::Header::read_from(&mut read)?;
        let offset_to_first_point_in_file = raw_header.offset_to_point_data as u64;
        let size_of_point_in_file = raw_header.point_data_record_length as u64;
        let point_offsets = Vector3::new(
            raw_header.x_offset,
            raw_header.y_offset,
            raw_header.z_offset,
        );
        let point_scales = Vector3::new(
            raw_header.x_scale_factor,
            raw_header.y_scale_factor,
            raw_header.z_scale_factor,
        );

        let header = Header::from_raw(raw_header)?;
        let metadata: LASMetadata = header.clone().into();
        let point_layout = point_layout_from_las_point_format(header.point_format())?;

        read.seek(SeekFrom::Start(offset_to_first_point_in_file as u64))?;

        Ok(Self {
            reader: read,
            metadata: metadata,
            layout: point_layout,
            current_point_index: 0,
            point_offsets,
            point_scales,
            offset_to_first_point_in_file,
            size_of_point_in_file,
        })
    }

    fn read_chunk_default_layout(
        &mut self,
        chunk_buffer: &mut [u8],
        num_points_in_chunk: usize,
    ) -> Result<()> {
        let mut buffer_cursor = Cursor::new(chunk_buffer);

        let format = Format::new(self.metadata.point_format())?;

        for _ in 0..num_points_in_chunk {
            // XYZ
            let local_x = self.reader.read_u32::<LittleEndian>()?;
            let local_y = self.reader.read_u32::<LittleEndian>()?;
            let local_z = self.reader.read_u32::<LittleEndian>()?;
            let global_x = (local_x as f64 * self.point_scales.x) + self.point_offsets.x;
            let global_y = (local_y as f64 * self.point_scales.y) + self.point_offsets.y;
            let global_z = (local_z as f64 * self.point_scales.z) + self.point_offsets.z;
            buffer_cursor.write_f64::<NativeEndian>(global_x)?;
            buffer_cursor.write_f64::<NativeEndian>(global_y)?;
            buffer_cursor.write_f64::<NativeEndian>(global_z)?;

            // Intensity
            buffer_cursor.write_i16::<NativeEndian>(self.reader.read_i16::<LittleEndian>()?)?;

            // Bit attributes
            if self.metadata.point_format() > 5 {
                let bit_attributes_first_byte = self.reader.read_u8()?;
                let bit_attributes_second_byte = self.reader.read_u8()?;

                let return_number = bit_attributes_first_byte & 0b1111;
                let number_of_returns = (bit_attributes_first_byte >> 4) & 0b1111;
                let classification_flags = bit_attributes_second_byte & 0b1111;
                let scanner_channel = (bit_attributes_second_byte >> 4) & 0b11;
                let scan_direction_flag = (bit_attributes_second_byte >> 6) & 0b1;
                let edge_of_flight_line = (bit_attributes_second_byte >> 7) & 0b1;

                buffer_cursor.write_u8(return_number)?;
                buffer_cursor.write_u8(number_of_returns)?;
                buffer_cursor.write_u8(classification_flags)?;
                buffer_cursor.write_u8(scanner_channel)?;
                buffer_cursor.write_u8(scan_direction_flag)?;
                buffer_cursor.write_u8(edge_of_flight_line)?;
            } else {
                let bit_attributes = self.reader.read_u8()?;
                let return_number = bit_attributes & 0b111;
                let number_of_returns = (bit_attributes >> 3) & 0b111;
                let scan_direction_flag = (bit_attributes >> 6) & 0b1;
                let edge_of_flight_line = (bit_attributes >> 7) & 0b1;

                buffer_cursor.write_u8(return_number)?;
                buffer_cursor.write_u8(number_of_returns)?;
                buffer_cursor.write_u8(scan_direction_flag)?;
                buffer_cursor.write_u8(edge_of_flight_line)?;
            }

            // Classification
            buffer_cursor.write_u8(self.reader.read_u8()?)?;

            // User data in format > 5, scan angle rank in format <= 5
            buffer_cursor.write_u8(self.reader.read_u8()?)?;

            if self.metadata.point_format() <= 5 {
                // User data
                buffer_cursor.write_u8(self.reader.read_u8()?)?;
            } else {
                // Scan angle
                buffer_cursor.write_i16::<NativeEndian>(self.reader.read_i16::<LittleEndian>()?)?;
            }

            // Point source ID
            buffer_cursor.write_u16::<NativeEndian>(self.reader.read_u16::<LittleEndian>()?)?;

            // Format 0 is done here, the other formats are handled now

            if format.has_gps_time {
                buffer_cursor.write_f64::<NativeEndian>(self.reader.read_f64::<LittleEndian>()?)?;
            }

            if format.has_color {
                buffer_cursor.write_u16::<NativeEndian>(self.reader.read_u16::<LittleEndian>()?)?;
                buffer_cursor.write_u16::<NativeEndian>(self.reader.read_u16::<LittleEndian>()?)?;
                buffer_cursor.write_u16::<NativeEndian>(self.reader.read_u16::<LittleEndian>()?)?;
            }

            if format.has_nir {
                buffer_cursor.write_u16::<NativeEndian>(self.reader.read_u16::<LittleEndian>()?)?;
            }

            if format.has_waveform {
                buffer_cursor.write_u8(self.reader.read_u8()?)?;
                buffer_cursor.write_u64::<NativeEndian>(self.reader.read_u64::<LittleEndian>()?)?;
                buffer_cursor.write_u32::<NativeEndian>(self.reader.read_u32::<LittleEndian>()?)?;
                buffer_cursor.write_f32::<NativeEndian>(self.reader.read_f32::<LittleEndian>()?)?;
                buffer_cursor.write_f32::<NativeEndian>(self.reader.read_f32::<LittleEndian>()?)?;
                buffer_cursor.write_f32::<NativeEndian>(self.reader.read_f32::<LittleEndian>()?)?;
                buffer_cursor.write_f32::<NativeEndian>(self.reader.read_f32::<LittleEndian>()?)?;
            }
        }

        Ok(())
    }

    fn read_chunk_custom_layout(
        &mut self,
        chunk_buffer: &mut [u8],
        num_points_in_chunk: usize,
        target_layout: &PointLayout,
    ) -> Result<()> {
        //let mut buffer_cursor = Cursor::new(chunk_buffer);

        let source_format = Format::new(self.metadata.point_format())?;

        // This probably works best by introducing a type that stores all information needed for reading and writing a single
        // attribute:
        //   - does the source format of the LAS file have this attribute?
        //   - does the target layout have this attribute?
        //   - if the target layout has the attribute, we may need an attribute converter
        //   - if the target layout has the attribute, we need the byte offset of the attribute to the start of the point record within the point layout
        //
        // With this information, we can build a bunch of these objects and execute the I/O operations with them, should be more readable

        fn get_attribute_parser(
            name: &str,
            source_layout: &PointLayout,
            target_layout: &PointLayout,
        ) -> Option<(usize, usize, Option<AttributeConversionFn>)> {
            target_layout
                .get_attribute_by_name(name)
                .map_or(None, |target_attribute| {
                    let converter =
                        source_layout
                            .get_attribute_by_name(name)
                            .and_then(|source_attribute| {
                                get_converter_for_attributes(
                                    &source_attribute.into(),
                                    &target_attribute.into(),
                                )
                            });
                    let offset_of_attribute = target_attribute.offset() as usize;
                    let size_of_attribute = target_attribute.size() as usize;
                    Some((offset_of_attribute, size_of_attribute, converter))
                })
        }

        let target_position_parser =
            get_attribute_parser(attributes::POSITION_3D.name(), &self.layout, target_layout);
        let target_intensity_parser =
            get_attribute_parser(attributes::INTENSITY.name(), &self.layout, target_layout);
        let target_return_number_parser = get_attribute_parser(
            attributes::RETURN_NUMBER.name(),
            &self.layout,
            target_layout,
        );
        let target_number_of_returns_parser = get_attribute_parser(
            attributes::NUMBER_OF_RETURNS.name(),
            &self.layout,
            target_layout,
        );
        let target_classification_flags_parser = get_attribute_parser(
            attributes::CLASSIFICATION_FLAGS.name(),
            &self.layout,
            target_layout,
        );
        let target_scanner_channel_parser = get_attribute_parser(
            attributes::SCANNER_CHANNEL.name(),
            &self.layout,
            target_layout,
        );
        let target_scan_direction_flag_parser = get_attribute_parser(
            attributes::SCAN_DIRECTION_FLAG.name(),
            &self.layout,
            target_layout,
        );
        let target_eof_parser = get_attribute_parser(
            attributes::EDGE_OF_FLIGHT_LINE.name(),
            &self.layout,
            target_layout,
        );
        let target_classification_parser = get_attribute_parser(
            attributes::CLASSIFICATION.name(),
            &self.layout,
            target_layout,
        );
        let target_scan_angle_rank_parser = get_attribute_parser(
            attributes::SCAN_ANGLE_RANK.name(),
            &self.layout,
            target_layout,
        );
        let target_user_data_parser =
            get_attribute_parser(attributes::USER_DATA.name(), &self.layout, target_layout);
        let target_point_source_id_parser = get_attribute_parser(
            attributes::POINT_SOURCE_ID.name(),
            &self.layout,
            target_layout,
        );
        let target_gps_time_parser =
            get_attribute_parser(attributes::GPS_TIME.name(), &self.layout, target_layout);
        let target_color_parser =
            get_attribute_parser(attributes::COLOR_RGB.name(), &self.layout, target_layout);
        let target_nir_parser =
            get_attribute_parser(attributes::NIR.name(), &self.layout, target_layout);
        let target_wave_packet_index_parser = get_attribute_parser(
            attributes::WAVE_PACKET_DESCRIPTOR_INDEX.name(),
            &self.layout,
            target_layout,
        );
        let target_waveform_byte_offset_parser = get_attribute_parser(
            attributes::WAVEFORM_DATA_OFFSET.name(),
            &self.layout,
            target_layout,
        );
        let target_waveform_packet_size_parser = get_attribute_parser(
            attributes::WAVEFORM_PACKET_SIZE.name(),
            &self.layout,
            target_layout,
        );
        let target_waveform_return_point_parser = get_attribute_parser(
            attributes::RETURN_POINT_WAVEFORM_LOCATION.name(),
            &self.layout,
            target_layout,
        );
        let target_waveform_parameters_parser = get_attribute_parser(
            attributes::WAVEFORM_PARAMETERS.name(),
            &self.layout,
            target_layout,
        );

        // TODO Waveform stuff...

        // TODO I'm not convinced that it is faster to check if we can skip certain attributes than it is to simply
        // read all data that the LAS file has and only extract the relevant attributes from it...

        let target_point_size = target_layout.size_of_point_entry() as usize;

        for point_index in 0..num_points_in_chunk {
            let start_of_target_point_in_chunk = point_index * target_point_size;

            if let Some((target_position_offset, position_size, maybe_converter)) =
                target_position_parser
            {
                let world_space_pos = self.read_next_world_space_position()?;
                let world_space_pos_slice = unsafe { view_raw_bytes(&world_space_pos) };

                let pos_start = start_of_target_point_in_chunk + target_position_offset;
                let pos_end = pos_start + position_size;
                let target_slice = &mut chunk_buffer[pos_start..pos_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(world_space_pos_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(world_space_pos_slice);
                }
            } else {
                self.reader.seek(SeekFrom::Current(12))?;
            }

            if let Some((target_intensity_offset, intensity_size, maybe_converter)) =
                target_intensity_parser
            {
                // TODO We can take this whole block of code and store it inside an object to make it easier to read
                // Only question is how we handle the self.read_next_ATTRIBUTENAME() calls...
                let intensity = self.read_next_intensity()?;
                let intensity_slice = unsafe { view_raw_bytes(&intensity) };

                let intensity_start = start_of_target_point_in_chunk + target_intensity_offset;
                let intensity_end = intensity_start + intensity_size;
                let target_slice = &mut chunk_buffer[intensity_start..intensity_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(intensity_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(intensity_slice);
                }
            } else {
                self.reader.seek(SeekFrom::Current(2))?;
            }

            let bit_attributes = self.read_next_bit_attributes(&source_format)?;
            if let Some((offset, size, maybe_converter)) = target_return_number_parser {
                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                let return_number_byte = match &bit_attributes {
                    BitAttributes::Regular(data) => [data.return_number],
                    BitAttributes::Extended(data) => [data.return_number],
                };
                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(&return_number_byte[..], target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(&return_number_byte[..]);
                }
            }
            if let Some((offset, size, maybe_converter)) = target_number_of_returns_parser {
                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                let number_of_returns_byte = match &bit_attributes {
                    BitAttributes::Regular(data) => [data.number_of_returns],
                    BitAttributes::Extended(data) => [data.number_of_returns],
                };
                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(&number_of_returns_byte[..], target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(&number_of_returns_byte[..]);
                }
            }
            if let Some((offset, size, maybe_converter)) = target_classification_flags_parser {
                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                let classification_flags_byte = match &bit_attributes {
                    BitAttributes::Regular(_) => [0; 1],
                    BitAttributes::Extended(data) => [data.classification_flags],
                };
                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(&classification_flags_byte[..], target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(&classification_flags_byte[..]);
                }
            }
            if let Some((offset, size, maybe_converter)) = target_scanner_channel_parser {
                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                let scanner_channel_byte = match &bit_attributes {
                    BitAttributes::Regular(_) => [0; 1],
                    BitAttributes::Extended(data) => [data.scanner_channel],
                };
                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(&scanner_channel_byte[..], target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(&scanner_channel_byte[..]);
                }
            }
            if let Some((offset, size, maybe_converter)) = target_scan_direction_flag_parser {
                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                let scan_direction_flag_byte = match &bit_attributes {
                    BitAttributes::Regular(data) => [data.scan_direction_flag],
                    BitAttributes::Extended(data) => [data.scan_direction_flag],
                };
                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(&scan_direction_flag_byte[..], target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(&scan_direction_flag_byte[..]);
                }
            }
            if let Some((offset, size, maybe_converter)) = target_eof_parser {
                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                let eof_byte = match &bit_attributes {
                    BitAttributes::Regular(data) => [data.edge_of_flight_line],
                    BitAttributes::Extended(data) => [data.edge_of_flight_line],
                };
                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(&eof_byte[..], target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(&eof_byte[..]);
                }
            }

            if let Some((offset, size, maybe_converter)) = target_classification_parser {
                let classification = self.read_next_classification()?;

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(&[classification], target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(&[classification]);
                }
            } else {
                self.reader.seek(SeekFrom::Current(1))?;
            }

            if !source_format.is_extended {
                let scan_angle_rank = self.reader.read_i8()?;
                if let Some((offset, size, maybe_converter)) = target_scan_angle_rank_parser {
                    let scan_angle_rank_slice = unsafe { view_raw_bytes(&scan_angle_rank) };

                    let target_slice_start = start_of_target_point_in_chunk + offset;
                    let target_slice_end = target_slice_start + size;
                    let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                    if let Some(converter) = maybe_converter {
                        unsafe {
                            converter(scan_angle_rank_slice, target_slice);
                        }
                    } else {
                        target_slice.copy_from_slice(scan_angle_rank_slice);
                    }
                }

                let user_data = self.reader.read_u8()?;
                if let Some((offset, size, maybe_converter)) = target_user_data_parser {
                    let target_slice_start = start_of_target_point_in_chunk + offset;
                    let target_slice_end = target_slice_start + size;
                    let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                    if let Some(converter) = maybe_converter {
                        unsafe {
                            converter(&[user_data], target_slice);
                        }
                    } else {
                        target_slice.copy_from_slice(&[user_data]);
                    }
                }
            } else {
                let user_data = self.reader.read_u8()?;
                if let Some((offset, size, maybe_converter)) = target_user_data_parser {
                    let target_slice_start = start_of_target_point_in_chunk + offset;
                    let target_slice_end = target_slice_start + size;
                    let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                    if let Some(converter) = maybe_converter {
                        unsafe {
                            converter(&[user_data], target_slice);
                        }
                    } else {
                        target_slice.copy_from_slice(&[user_data]);
                    }
                }

                let scan_angle = self.reader.read_i16::<LittleEndian>()?;
                if let Some((offset, size, maybe_converter)) = target_scan_angle_rank_parser {
                    let scan_angle_bytes = unsafe { view_raw_bytes(&scan_angle) };

                    let target_slice_start = start_of_target_point_in_chunk + offset;
                    let target_slice_end = target_slice_start + size;
                    let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                    if let Some(converter) = maybe_converter {
                        unsafe {
                            converter(scan_angle_bytes, target_slice);
                        }
                    } else {
                        target_slice.copy_from_slice(scan_angle_bytes);
                    }
                }
            }

            if let Some((offset, size, maybe_converter)) = target_point_source_id_parser {
                let point_source_id = self.read_next_point_source_id()?;
                let point_source_id_slice = unsafe { view_raw_bytes(&point_source_id) };

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(point_source_id_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(point_source_id_slice);
                }
            } else {
                self.reader.seek(SeekFrom::Current(2))?;
            }

            if let Some((offset, size, maybe_converter)) = target_gps_time_parser {
                let gps_time = self.read_next_gps_time_or_default(&source_format)?;
                let gps_time_slice = unsafe { view_raw_bytes(&gps_time) };

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(gps_time_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(gps_time_slice);
                }
            } else if source_format.has_gps_time {
                self.reader.seek(SeekFrom::Current(8))?;
            }

            if let Some((offset, size, maybe_converter)) = target_color_parser {
                let color = self.read_next_color_or_default(&source_format)?;
                let color_slice = unsafe { view_raw_bytes(&color) };

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(color_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(color_slice);
                }
            } else if source_format.has_color {
                self.reader.seek(SeekFrom::Current(6))?;
            }

            if let Some((offset, size, maybe_converter)) = target_nir_parser {
                let nir = self.read_next_nir_or_default(&source_format)?;
                let nir_slice = unsafe { view_raw_bytes(&nir) };

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(nir_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(nir_slice);
                }
            } else if source_format.has_nir {
                self.reader.seek(SeekFrom::Current(2))?;
            }

            if let Some((offset, size, maybe_converter)) = target_wave_packet_index_parser {
                let wpi = self.read_next_wave_packet_index_or_default(&source_format)?;

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(&[wpi], target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(&[wpi]);
                }
            } else if source_format.has_waveform {
                self.reader.seek(SeekFrom::Current(1))?;
            }

            if let Some((offset, size, maybe_converter)) = target_waveform_byte_offset_parser {
                let wbo = self.read_next_waveform_byte_offset(&source_format)?;
                let wbo_slice = unsafe { view_raw_bytes(&wbo) };

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(wbo_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(wbo_slice);
                }
            } else if source_format.has_waveform {
                self.reader.seek(SeekFrom::Current(8))?;
            }

            if let Some((offset, size, maybe_converter)) = target_waveform_packet_size_parser {
                let wps = self.read_next_waveform_packet_size(&source_format)?;
                let wps_slice = unsafe { view_raw_bytes(&wps) };

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(wps_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(wps_slice);
                }
            } else if source_format.has_waveform {
                self.reader.seek(SeekFrom::Current(4))?;
            }

            if let Some((offset, size, maybe_converter)) = target_waveform_return_point_parser {
                let waveform_location = self.read_next_waveform_location(&source_format)?;
                let waveform_location_slice = unsafe { view_raw_bytes(&waveform_location) };

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(waveform_location_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(waveform_location_slice);
                }
            } else if source_format.has_waveform {
                self.reader.seek(SeekFrom::Current(4))?;
            }

            if let Some((offset, size, maybe_converter)) = target_waveform_parameters_parser {
                let waveform_params = self.read_next_waveform_parameters(&source_format)?;
                let waveform_params_slice = unsafe { view_raw_bytes(&waveform_params) };

                let target_slice_start = start_of_target_point_in_chunk + offset;
                let target_slice_end = target_slice_start + size;
                let target_slice = &mut chunk_buffer[target_slice_start..target_slice_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(waveform_params_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(waveform_params_slice);
                }
            } else if source_format.has_waveform {
                self.reader.seek(SeekFrom::Current(12))?;
            }
        }

        Ok(())
    }

    fn read_into_default_layout(
        &mut self,
        point_buffer: &mut dyn PointBufferWriteable,
        count: usize,
    ) -> Result<usize> {
        let num_points_to_read = usize::min(count, self.remaining_points());
        if num_points_to_read == 0 {
            return Ok(0);
        }

        // Read into chunks of a fixed size. Within each chunk, read all data into an untyped buffer
        // then push the untyped data into 'buffer'
        let chunk_size = 50_000;
        let point_size = self.layout.size_of_point_entry() as usize;
        let chunk_bytes = point_size as usize * chunk_size;
        let num_chunks = (num_points_to_read + chunk_size - 1) / chunk_size;
        let mut points_chunk: Vec<u8> = vec![0; chunk_bytes];

        for chunk_index in 0..num_chunks {
            let points_in_chunk =
                std::cmp::min(chunk_size, num_points_to_read - (chunk_index * chunk_size));
            let bytes_in_chunk = points_in_chunk * point_size;

            self.read_chunk_default_layout(&mut points_chunk[..], points_in_chunk)?;

            point_buffer.push_points_interleaved(&InterleavedPointView::from_raw_slice(
                &points_chunk[0..bytes_in_chunk],
                self.layout.clone(),
            ));
        }

        self.current_point_index += num_points_to_read;

        Ok(num_points_to_read)
    }

    fn read_into_custom_layout(
        &mut self,
        point_buffer: &mut dyn PointBufferWriteable,
        count: usize,
    ) -> Result<usize> {
        let num_points_to_read = usize::min(count, self.remaining_points());
        if num_points_to_read == 0 {
            return Ok(0);
        }

        // Read in interleaved chunks, even if the `point_buffer` is not interleaved. `push_points_interleaved` will
        // handle the memory transpose in this case
        let chunk_size = 50_000;
        let point_size = point_buffer.point_layout().size_of_point_entry() as usize;
        let chunk_bytes = point_size * chunk_size;
        let num_chunks = (num_points_to_read + chunk_size - 1) / chunk_size;
        let mut points_chunk: Vec<u8> = vec![0; chunk_bytes];

        for chunk_index in 0..num_chunks {
            let points_in_chunk =
                std::cmp::min(chunk_size, num_points_to_read - (chunk_index * chunk_size));
            let bytes_in_chunk = points_in_chunk * point_size;

            self.read_chunk_custom_layout(
                &mut points_chunk[..],
                points_in_chunk,
                point_buffer.point_layout(),
            )?;

            point_buffer.push_points_interleaved(&InterleavedPointView::from_raw_slice(
                &points_chunk[0..bytes_in_chunk],
                point_buffer.point_layout().clone(),
            ));
        }

        self.current_point_index += num_points_to_read;

        Ok(num_points_to_read)
    }

    /// Read the next position, converted into world space of the current LAS file
    fn read_next_world_space_position(&mut self) -> Result<Vector3<f64>> {
        let local_x = self.reader.read_u32::<LittleEndian>()?;
        let local_y = self.reader.read_u32::<LittleEndian>()?;
        let local_z = self.reader.read_u32::<LittleEndian>()?;
        let global_x = (local_x as f64 * self.point_scales.x) + self.point_offsets.x;
        let global_y = (local_y as f64 * self.point_scales.y) + self.point_offsets.y;
        let global_z = (local_z as f64 * self.point_scales.z) + self.point_offsets.z;
        Ok(Vector3::new(global_x, global_y, global_z))
    }

    /// Read the next intensity from the current LAS file
    fn read_next_intensity(&mut self) -> Result<u16> {
        Ok(self.reader.read_u16::<LittleEndian>()?)
    }

    /// Read the next bit flag attributes from the current LAS file
    fn read_next_bit_attributes(&mut self, las_format: &Format) -> Result<BitAttributes> {
        if las_format.is_extended {
            let low_byte = self.reader.read_u8()?;
            let high_byte = self.reader.read_u8()?;

            Ok(BitAttributes::Extended(BitAttributesExtended {
                return_number: low_byte & 0b1111,
                number_of_returns: (low_byte >> 4) & 0b1111,
                classification_flags: high_byte & 0b1111,
                scanner_channel: (high_byte >> 4) & 0b11,
                scan_direction_flag: (high_byte >> 6) & 0b1,
                edge_of_flight_line: (high_byte >> 7) & 0b1,
            }))
        } else {
            let byte = self.reader.read_u8()?;
            Ok(BitAttributes::Regular(BitAttributesRegular {
                return_number: byte & 0b111,
                number_of_returns: (byte >> 3) & 0b111,
                scan_direction_flag: (byte >> 6) & 0b1,
                edge_of_flight_line: (byte >> 7) & 0b1,
            }))
        }
    }

    fn read_next_classification(&mut self) -> Result<u8> {
        Ok(self.reader.read_u8()?)
    }

    fn read_next_point_source_id(&mut self) -> Result<u16> {
        Ok(self.reader.read_u16::<LittleEndian>()?)
    }

    fn read_next_gps_time_or_default(&mut self, las_format: &Format) -> Result<f64> {
        if !las_format.has_gps_time {
            Ok(Default::default())
        } else {
            Ok(self.reader.read_f64::<LittleEndian>()?)
        }
    }

    fn read_next_color_or_default(&mut self, las_format: &Format) -> Result<Vector3<u16>> {
        if !las_format.has_color {
            Ok(Default::default())
        } else {
            let r = self.reader.read_u16::<LittleEndian>()?;
            let g = self.reader.read_u16::<LittleEndian>()?;
            let b = self.reader.read_u16::<LittleEndian>()?;
            Ok(Vector3::new(r, g, b))
        }
    }

    fn read_next_nir_or_default(&mut self, las_format: &Format) -> Result<u16> {
        if !las_format.has_nir {
            Ok(Default::default())
        } else {
            Ok(self.reader.read_u16::<LittleEndian>()?)
        }
    }

    fn read_next_wave_packet_index_or_default(&mut self, las_format: &Format) -> Result<u8> {
        if !las_format.has_waveform {
            Ok(Default::default())
        } else {
            Ok(self.reader.read_u8()?)
        }
    }

    fn read_next_waveform_byte_offset(&mut self, las_format: &Format) -> Result<u64> {
        if !las_format.has_waveform {
            Ok(Default::default())
        } else {
            Ok(self.reader.read_u64::<LittleEndian>()?)
        }
    }

    fn read_next_waveform_packet_size(&mut self, las_format: &Format) -> Result<u32> {
        if !las_format.has_waveform {
            Ok(Default::default())
        } else {
            Ok(self.reader.read_u32::<LittleEndian>()?)
        }
    }

    fn read_next_waveform_location(&mut self, las_format: &Format) -> Result<f32> {
        if !las_format.has_waveform {
            Ok(Default::default())
        } else {
            Ok(self.reader.read_f32::<LittleEndian>()?)
        }
    }

    fn read_next_waveform_parameters(&mut self, las_format: &Format) -> Result<Vector3<f32>> {
        if !las_format.has_waveform {
            Ok(Default::default())
        } else {
            let px = self.reader.read_f32::<LittleEndian>()?;
            let py = self.reader.read_f32::<LittleEndian>()?;
            let pz = self.reader.read_f32::<LittleEndian>()?;
            Ok(Vector3::new(px, py, pz))
        }
    }
}

impl<T: Read + Seek> LASReaderBase for RawLASReader<T> {
    fn remaining_points(&self) -> usize {
        self.metadata.point_count() - self.current_point_index
    }
}

impl<T: Read + Seek> PointReader for RawLASReader<T> {
    fn read(&mut self, count: usize) -> Result<Box<dyn pasture_core::containers::PointBuffer>> {
        let num_points_to_read = usize::min(count, self.remaining_points());
        let mut buffer =
            InterleavedVecPointStorage::with_capacity(num_points_to_read, self.layout.clone());

        self.read_into(&mut buffer, num_points_to_read)?;

        Ok(Box::new(buffer))
    }

    fn read_into(
        &mut self,
        point_buffer: &mut dyn PointBufferWriteable,
        count: usize,
    ) -> Result<usize> {
        if *point_buffer.point_layout() != self.layout {
            self.read_into_custom_layout(point_buffer, count)
        } else {
            self.read_into_default_layout(point_buffer, count)
        }
    }

    fn get_metadata(&self) -> &dyn Metadata {
        &self.metadata
    }

    fn get_default_point_layout(&self) -> &PointLayout {
        &self.layout
    }
}

impl<T: Read + Seek> SeekToPoint for RawLASReader<T> {
    fn seek_point(&mut self, position: SeekFrom) -> Result<usize> {
        let new_position = match position {
            SeekFrom::Start(from_start) => from_start as i64,
            SeekFrom::End(from_end) => self.metadata.point_count() as i64 + from_end,
            SeekFrom::Current(from_current) => self.current_point_index as i64 + from_current,
        };
        if new_position < 0 {
            panic!("RawLASReader::seek_point: It is an error to seek to a point position smaller than zero!");
        }
        let clamped_position =
            std::cmp::min(self.metadata.point_count() as i64, new_position) as usize;

        if self.current_point_index != clamped_position {
            let position_within_file = self.offset_to_first_point_in_file
                + clamped_position as u64 * self.size_of_point_in_file;
            self.reader.seek(SeekFrom::Start(position_within_file))?;
            self.current_point_index = clamped_position;
        }

        Ok(self.current_point_index)
    }
}

pub(crate) struct RawLAZReader<'a, T: Read + Seek + Send + 'a> {
    reader: LasZipDecompressor<'a, T>,
    metadata: LASMetadata,
    layout: PointLayout,
    current_point_index: usize,
    point_offsets: Vector3<f64>,
    point_scales: Vector3<f64>,
    size_of_point_in_file: u64,
}

impl<'a, T: Read + Seek + Send + 'a> RawLAZReader<'a, T> {
    pub fn from_read(mut read: T) -> Result<Self> {
        let raw_header = raw::Header::read_from(&mut read)?;
        let offset_to_first_point_in_file = raw_header.offset_to_point_data as u64;
        let size_of_point_in_file = raw_header.point_data_record_length as u64;
        let number_of_vlrs = raw_header.number_of_variable_length_records;
        let point_offsets = Vector3::new(
            raw_header.x_offset,
            raw_header.y_offset,
            raw_header.z_offset,
        );
        let point_scales = Vector3::new(
            raw_header.x_scale_factor,
            raw_header.y_scale_factor,
            raw_header.z_scale_factor,
        );

        let mut header_builder = Builder::new(raw_header)?;
        // Read VLRs
        for _ in 0..number_of_vlrs {
            let vlr = las_rs::raw::Vlr::read_from(&mut read, false).map(Vlr::new)?;
            header_builder.vlrs.push(vlr);
        }
        // TODO Read EVLRs

        let header = header_builder.into_header()?;
        if header.point_format().has_waveform {
            return Err(anyhow!(
                "Compressed LAZ files with wave packet data are currently not supported!"
            ));
        }
        if header.point_format().is_extended {
            return Err(anyhow!(
                "Compressed LAZ files with extended formats (6-10) are currently not supported!"
            ));
        }

        let metadata: LASMetadata = header.clone().into();
        let point_layout = point_layout_from_las_point_format(header.point_format())?;

        read.seek(SeekFrom::Start(offset_to_first_point_in_file as u64))?;

        let laszip_vlr = match header.vlrs().iter().find(|vlr| is_laszip_vlr(*vlr)) {
            None => Err(anyhow!(
                "RawLAZReader::new: LAZ variable length record not found in file!"
            )),
            Some(ref vlr) => {
                let laz_record =
                    laz::las::laszip::LazVlr::from_buffer(&vlr.data).map_err(map_laz_err)?;
                Ok(laz_record)
            }
        }?;
        let reader = LasZipDecompressor::new(read, laszip_vlr).map_err(map_laz_err)?;

        Ok(Self {
            reader,
            metadata: metadata,
            layout: point_layout,
            current_point_index: 0,
            point_offsets,
            point_scales,
            size_of_point_in_file,
        })
    }

    fn read_chunk_default_layout(
        &mut self,
        chunk_buffer: &mut [u8],
        decompression_buffer: &mut [u8],
        num_points_in_chunk: usize,
    ) -> Result<()> {
        let bytes_in_chunk = num_points_in_chunk * self.size_of_point_in_file as usize;
        let target_point_size = self.layout.size_of_point_entry() as usize;
        let las_format = Format::new(self.metadata.point_format())?;

        self.reader
            .decompress_many(&mut decompression_buffer[0..bytes_in_chunk])?;
        let mut decompression_chunk_cursor = Cursor::new(decompression_buffer);
        let mut target_chunk_cursor = Cursor::new(chunk_buffer);

        // Convert the decompressed points - which have XYZ as u32 - into the target layout
        for point_index in 0..num_points_in_chunk {
            let local_x = decompression_chunk_cursor.read_u32::<LittleEndian>()?;
            let local_y = decompression_chunk_cursor.read_u32::<LittleEndian>()?;
            let local_z = decompression_chunk_cursor.read_u32::<LittleEndian>()?;
            let global_x = (local_x as f64 * self.point_scales.x) + self.point_offsets.x;
            let global_y = (local_y as f64 * self.point_scales.y) + self.point_offsets.y;
            let global_z = (local_z as f64 * self.point_scales.z) + self.point_offsets.z;
            target_chunk_cursor.write_f64::<NativeEndian>(global_x)?;
            target_chunk_cursor.write_f64::<NativeEndian>(global_y)?;
            target_chunk_cursor.write_f64::<NativeEndian>(global_z)?;

            // Intensity
            target_chunk_cursor.write_i16::<NativeEndian>(
                decompression_chunk_cursor.read_i16::<LittleEndian>()?,
            )?;

            // Bit attributes
            if las_format.is_extended {
                let bit_attributes_first_byte = decompression_chunk_cursor.read_u8()?;
                let bit_attributes_second_byte = decompression_chunk_cursor.read_u8()?;

                let return_number = bit_attributes_first_byte & 0b1111;
                let number_of_returns = (bit_attributes_first_byte >> 4) & 0b1111;
                let classification_flags = bit_attributes_second_byte & 0b1111;
                let scanner_channel = (bit_attributes_second_byte >> 4) & 0b11;
                let scan_direction_flag = (bit_attributes_second_byte >> 6) & 0b1;
                let edge_of_flight_line = (bit_attributes_second_byte >> 7) & 0b1;

                target_chunk_cursor.write_u8(return_number)?;
                target_chunk_cursor.write_u8(number_of_returns)?;
                target_chunk_cursor.write_u8(classification_flags)?;
                target_chunk_cursor.write_u8(scanner_channel)?;
                target_chunk_cursor.write_u8(scan_direction_flag)?;
                target_chunk_cursor.write_u8(edge_of_flight_line)?;
            } else {
                let bit_attributes = decompression_chunk_cursor.read_u8()?;
                let return_number = bit_attributes & 0b111;
                let number_of_returns = (bit_attributes >> 3) & 0b111;
                let scan_direction_flag = (bit_attributes >> 6) & 0b1;
                let edge_of_flight_line = (bit_attributes >> 7) & 0b1;

                target_chunk_cursor.write_u8(return_number)?;
                target_chunk_cursor.write_u8(number_of_returns)?;
                target_chunk_cursor.write_u8(scan_direction_flag)?;
                target_chunk_cursor.write_u8(edge_of_flight_line)?;
            }

            // Classification
            target_chunk_cursor.write_u8(decompression_chunk_cursor.read_u8()?)?;

            // User data in format > 5, scan angle rank in format <= 5
            target_chunk_cursor.write_u8(decompression_chunk_cursor.read_u8()?)?;

            if self.metadata.point_format() <= 5 {
                // User data
                target_chunk_cursor.write_u8(decompression_chunk_cursor.read_u8()?)?;
            } else {
                // Scan angle
                target_chunk_cursor.write_i16::<NativeEndian>(
                    decompression_chunk_cursor.read_i16::<LittleEndian>()?,
                )?;
            }

            // Point source ID
            target_chunk_cursor.write_u16::<NativeEndian>(
                decompression_chunk_cursor.read_u16::<LittleEndian>()?,
            )?;

            // Format 0 is done here, the other formats are handled now

            if las_format.has_gps_time {
                target_chunk_cursor.write_f64::<NativeEndian>(
                    decompression_chunk_cursor.read_f64::<LittleEndian>()?,
                )?;
            }

            if las_format.has_color {
                target_chunk_cursor.write_u16::<NativeEndian>(
                    decompression_chunk_cursor.read_u16::<LittleEndian>()?,
                )?;
                target_chunk_cursor.write_u16::<NativeEndian>(
                    decompression_chunk_cursor.read_u16::<LittleEndian>()?,
                )?;
                target_chunk_cursor.write_u16::<NativeEndian>(
                    decompression_chunk_cursor.read_u16::<LittleEndian>()?,
                )?;
            }

            if las_format.has_nir {
                target_chunk_cursor.write_u16::<NativeEndian>(
                    decompression_chunk_cursor.read_u16::<LittleEndian>()?,
                )?;
            }

            if las_format.has_waveform {
                target_chunk_cursor.write_u8(decompression_chunk_cursor.read_u8()?)?;
                target_chunk_cursor.write_u64::<NativeEndian>(
                    decompression_chunk_cursor.read_u64::<LittleEndian>()?,
                )?;
                target_chunk_cursor.write_u32::<NativeEndian>(
                    decompression_chunk_cursor.read_u32::<LittleEndian>()?,
                )?;
                target_chunk_cursor.write_f32::<NativeEndian>(
                    decompression_chunk_cursor.read_f32::<LittleEndian>()?,
                )?;
                target_chunk_cursor.write_f32::<NativeEndian>(
                    decompression_chunk_cursor.read_f32::<LittleEndian>()?,
                )?;
                target_chunk_cursor.write_f32::<NativeEndian>(
                    decompression_chunk_cursor.read_f32::<LittleEndian>()?,
                )?;
                target_chunk_cursor.write_f32::<NativeEndian>(
                    decompression_chunk_cursor.read_f32::<LittleEndian>()?,
                )?;
            }
        }

        Ok(())
    }

    fn read_chunk_custom_layout(
        &mut self,
        chunk_buffer: &mut [u8],
        decompression_buffer: &mut [u8],
        num_points_in_chunk: usize,
        target_layout: &PointLayout,
    ) -> Result<()> {
        // HACK Not happy with how large this function is... But there are so many special
        // cases, I don't know how to clean it up at the moment. Maybe revise in future?
        let source_format = Format::new(self.metadata.point_format())?;

        fn get_attribute_parser(
            name: &str,
            source_layout: &PointLayout,
            target_layout: &PointLayout,
        ) -> Option<(usize, usize, Option<AttributeConversionFn>)> {
            target_layout
                .get_attribute_by_name(name)
                .map_or(None, |target_attribute| {
                    let converter =
                        source_layout
                            .get_attribute_by_name(name)
                            .and_then(|source_attribute| {
                                get_converter_for_attributes(
                                    &source_attribute.into(),
                                    &target_attribute.into(),
                                )
                            });
                    let offset_of_attribute = target_attribute.offset() as usize;
                    let size_of_attribute = target_attribute.size() as usize;
                    Some((offset_of_attribute, size_of_attribute, converter))
                })
        }

        let target_position_parser =
            get_attribute_parser(attributes::POSITION_3D.name(), &self.layout, target_layout);
        let target_intensity_parser =
            get_attribute_parser(attributes::INTENSITY.name(), &self.layout, target_layout);
        let target_return_number_parser = get_attribute_parser(
            attributes::RETURN_NUMBER.name(),
            &self.layout,
            target_layout,
        );
        let target_number_of_returns_parser = get_attribute_parser(
            attributes::NUMBER_OF_RETURNS.name(),
            &self.layout,
            target_layout,
        );
        let target_classification_flags_parser = get_attribute_parser(
            attributes::CLASSIFICATION_FLAGS.name(),
            &self.layout,
            target_layout,
        );
        let target_scanner_channel_parser = get_attribute_parser(
            attributes::SCANNER_CHANNEL.name(),
            &self.layout,
            target_layout,
        );
        let target_scan_direction_flag_parser = get_attribute_parser(
            attributes::SCAN_DIRECTION_FLAG.name(),
            &self.layout,
            target_layout,
        );
        let target_eof_parser = get_attribute_parser(
            attributes::EDGE_OF_FLIGHT_LINE.name(),
            &self.layout,
            target_layout,
        );
        let target_classification_parser = get_attribute_parser(
            attributes::CLASSIFICATION.name(),
            &self.layout,
            target_layout,
        );
        let target_scan_angle_rank_parser = get_attribute_parser(
            attributes::SCAN_ANGLE_RANK.name(),
            &self.layout,
            target_layout,
        );
        let target_user_data_parser =
            get_attribute_parser(attributes::USER_DATA.name(), &self.layout, target_layout);
        let target_point_source_id_parser = get_attribute_parser(
            attributes::POINT_SOURCE_ID.name(),
            &self.layout,
            target_layout,
        );
        let target_gps_time_parser =
            get_attribute_parser(attributes::GPS_TIME.name(), &self.layout, target_layout);
        let target_color_parser =
            get_attribute_parser(attributes::COLOR_RGB.name(), &self.layout, target_layout);
        let target_nir_parser =
            get_attribute_parser(attributes::NIR.name(), &self.layout, target_layout);
        let target_wave_packet_index_parser = get_attribute_parser(
            attributes::WAVE_PACKET_DESCRIPTOR_INDEX.name(),
            &self.layout,
            target_layout,
        );
        let target_waveform_byte_offset_parser = get_attribute_parser(
            attributes::WAVEFORM_DATA_OFFSET.name(),
            &self.layout,
            target_layout,
        );
        let target_waveform_packet_size_parser = get_attribute_parser(
            attributes::WAVEFORM_PACKET_SIZE.name(),
            &self.layout,
            target_layout,
        );
        let target_waveform_return_point_parser = get_attribute_parser(
            attributes::RETURN_POINT_WAVEFORM_LOCATION.name(),
            &self.layout,
            target_layout,
        );
        let target_waveform_parameters_parser = get_attribute_parser(
            attributes::WAVEFORM_PARAMETERS.name(),
            &self.layout,
            target_layout,
        );

        let target_point_size = target_layout.size_of_point_entry() as usize;

        self.reader.decompress_many(
            &mut decompression_buffer
                [0..(num_points_in_chunk * self.size_of_point_in_file as usize)],
        )?;
        let mut decompressed_data = Cursor::new(decompression_buffer);

        fn run_parser<T>(
            decoder_fn: impl Fn(&mut Cursor<&mut [u8]>) -> Result<T>,
            maybe_parser: Option<(usize, usize, Option<AttributeConversionFn>)>,
            start_of_target_point_in_chunk: usize,
            size_of_attribute: Option<usize>,
            decompressed_data: &mut Cursor<&mut [u8]>,
            chunk_buffer: &mut [u8],
        ) -> Result<()> {
            if let Some((offset, size, maybe_converter)) = maybe_parser {
                let source_data = decoder_fn(decompressed_data)?;
                let source_slice = unsafe { view_raw_bytes(&source_data) };

                let pos_start = start_of_target_point_in_chunk + offset;
                let pos_end = pos_start + size;
                let target_slice = &mut chunk_buffer[pos_start..pos_end];

                if let Some(converter) = maybe_converter {
                    unsafe {
                        converter(source_slice, target_slice);
                    }
                } else {
                    target_slice.copy_from_slice(source_slice);
                }
            } else if let Some(bytes_to_skip) = size_of_attribute {
                decompressed_data.seek(SeekFrom::Current(bytes_to_skip as i64))?;
            }

            Ok(())
        }

        for point_index in 0..num_points_in_chunk {
            let start_of_target_point_in_chunk = point_index * target_point_size;

            run_parser(
                |buf| self.read_next_world_space_position(buf),
                target_position_parser,
                start_of_target_point_in_chunk,
                Some(12),
                &mut decompressed_data,
                chunk_buffer,
            )?;

            run_parser(
                |buf| Ok(buf.read_u16::<LittleEndian>()?),
                target_intensity_parser,
                start_of_target_point_in_chunk,
                Some(2),
                &mut decompressed_data,
                chunk_buffer,
            )?;

            let bit_attributes =
                self.read_next_bit_attributes(&mut decompressed_data, &source_format)?;
            run_parser(
                |_| Ok(bit_attributes.return_number()),
                target_return_number_parser,
                start_of_target_point_in_chunk,
                None,
                &mut decompressed_data,
                chunk_buffer,
            )?;
            run_parser(
                |_| Ok(bit_attributes.number_of_returns()),
                target_number_of_returns_parser,
                start_of_target_point_in_chunk,
                None,
                &mut decompressed_data,
                chunk_buffer,
            )?;
            run_parser(
                |_| Ok(bit_attributes.classification_flags_or_default()),
                target_classification_flags_parser,
                start_of_target_point_in_chunk,
                None,
                &mut decompressed_data,
                chunk_buffer,
            )?;
            run_parser(
                |_| Ok(bit_attributes.scanner_channel_or_default()),
                target_scanner_channel_parser,
                start_of_target_point_in_chunk,
                None,
                &mut decompressed_data,
                chunk_buffer,
            )?;
            run_parser(
                |_| Ok(bit_attributes.scan_direction_flag()),
                target_scan_direction_flag_parser,
                start_of_target_point_in_chunk,
                None,
                &mut decompressed_data,
                chunk_buffer,
            )?;
            run_parser(
                |_| Ok(bit_attributes.edge_of_flight_line()),
                target_eof_parser,
                start_of_target_point_in_chunk,
                None,
                &mut decompressed_data,
                chunk_buffer,
            )?;

            run_parser(
                |buf| Ok(buf.read_u8()?),
                target_classification_parser,
                start_of_target_point_in_chunk,
                Some(1),
                &mut decompressed_data,
                chunk_buffer,
            )?;

            if source_format.is_extended {
                // Extended LAS format has user data before scan angle
                run_parser(
                    |buf| Ok(buf.read_u8()?),
                    target_user_data_parser,
                    start_of_target_point_in_chunk,
                    Some(1),
                    &mut decompressed_data,
                    chunk_buffer,
                )?;

                run_parser(
                    |buf| Ok(buf.read_i16::<LittleEndian>()?),
                    target_scan_angle_rank_parser,
                    start_of_target_point_in_chunk,
                    Some(2),
                    &mut decompressed_data,
                    chunk_buffer,
                )?;
            } else {
                // Regular formats have scan angle rank before user data
                run_parser(
                    |buf| Ok(buf.read_i8()?),
                    target_scan_angle_rank_parser,
                    start_of_target_point_in_chunk,
                    Some(1),
                    &mut decompressed_data,
                    chunk_buffer,
                )?;

                run_parser(
                    |buf| Ok(buf.read_u8()?),
                    target_user_data_parser,
                    start_of_target_point_in_chunk,
                    Some(1),
                    &mut decompressed_data,
                    chunk_buffer,
                )?;
            }

            run_parser(
                |buf| Ok(buf.read_u16::<LittleEndian>()?),
                target_point_source_id_parser,
                start_of_target_point_in_chunk,
                Some(2),
                &mut decompressed_data,
                chunk_buffer,
            )?;

            let gps_bytes_in_current_format = if source_format.has_gps_time {
                Some(8)
            } else {
                None
            };
            run_parser(
                |buf| Ok(buf.read_f64::<LittleEndian>()?),
                target_gps_time_parser,
                start_of_target_point_in_chunk,
                gps_bytes_in_current_format,
                &mut decompressed_data,
                chunk_buffer,
            )?;

            let color_bytes_in_current_format = if source_format.has_color {
                Some(6)
            } else {
                None
            };
            run_parser(
                |buf| Self::read_next_colors_or_default(buf, &source_format),
                target_color_parser,
                start_of_target_point_in_chunk,
                color_bytes_in_current_format,
                &mut decompressed_data,
                chunk_buffer,
            )?;

            let nir_bytes_in_current_format = if source_format.has_nir { Some(2) } else { None };
            run_parser(
                |buf| Ok(buf.read_u16::<LittleEndian>()?),
                target_nir_parser,
                start_of_target_point_in_chunk,
                nir_bytes_in_current_format,
                &mut decompressed_data,
                chunk_buffer,
            )?;

            let wave_packet_index_bytes_in_current_format = if source_format.has_waveform {
                Some(1)
            } else {
                None
            };
            run_parser(
                |buf| Ok(buf.read_u8()?),
                target_wave_packet_index_parser,
                start_of_target_point_in_chunk,
                wave_packet_index_bytes_in_current_format,
                &mut decompressed_data,
                chunk_buffer,
            )?;
            let waveform_data_offset_bytes_in_current_format = if source_format.has_waveform {
                Some(8)
            } else {
                None
            };
            run_parser(
                |buf| Ok(buf.read_u64::<LittleEndian>()?),
                target_waveform_byte_offset_parser,
                start_of_target_point_in_chunk,
                waveform_data_offset_bytes_in_current_format,
                &mut decompressed_data,
                chunk_buffer,
            )?;
            let waveform_packet_bytes_in_current_format = if source_format.has_waveform {
                Some(4)
            } else {
                None
            };
            run_parser(
                |buf| Ok(buf.read_u32::<LittleEndian>()?),
                target_waveform_packet_size_parser,
                start_of_target_point_in_chunk,
                waveform_packet_bytes_in_current_format,
                &mut decompressed_data,
                chunk_buffer,
            )?;
            let waveform_location_bytes_in_current_format = if source_format.has_waveform {
                Some(4)
            } else {
                None
            };
            run_parser(
                |buf| Ok(buf.read_f32::<LittleEndian>()?),
                target_waveform_return_point_parser,
                start_of_target_point_in_chunk,
                waveform_location_bytes_in_current_format,
                &mut decompressed_data,
                chunk_buffer,
            )?;

            let waveform_params_bytes_in_current_format = if source_format.has_waveform {
                Some(12)
            } else {
                None
            };
            run_parser(
                |buf| Self::read_next_waveform_parameters_or_default(buf, &source_format),
                target_waveform_parameters_parser,
                start_of_target_point_in_chunk,
                waveform_params_bytes_in_current_format,
                &mut decompressed_data,
                chunk_buffer,
            )?;
        }

        Ok(())
    }

    fn read_into_default_layout(
        &mut self,
        point_buffer: &mut dyn PointBufferWriteable,
        count: usize,
    ) -> Result<usize> {
        let num_points_to_read = usize::min(count, self.remaining_points());
        if num_points_to_read == 0 {
            return Ok(0);
        }

        // Read into chunks of a fixed size. Within each chunk, read all data into an untyped buffer
        // then push the untyped data into 'buffer'
        let chunk_size = 50_000;
        let point_size = self.layout.size_of_point_entry() as usize;
        let chunk_bytes = point_size as usize * chunk_size;
        let num_chunks = (num_points_to_read + chunk_size - 1) / chunk_size;
        let mut points_chunk: Vec<u8> = vec![0; chunk_bytes];

        let decompression_chunk_size = self.size_of_point_in_file as usize * chunk_size;
        let mut decompression_chunk: Vec<u8> = vec![0; decompression_chunk_size];

        for chunk_index in 0..num_chunks {
            let points_in_chunk =
                std::cmp::min(chunk_size, num_points_to_read - (chunk_index * chunk_size));
            let bytes_in_chunk = points_in_chunk * point_size;

            self.read_chunk_default_layout(
                &mut points_chunk[..],
                &mut decompression_chunk[..],
                points_in_chunk,
            )?;

            point_buffer.push_points_interleaved(&InterleavedPointView::from_raw_slice(
                &points_chunk[0..bytes_in_chunk],
                self.layout.clone(),
            ));
        }

        self.current_point_index += num_points_to_read;

        Ok(num_points_to_read)
    }

    fn read_into_custom_layout(
        &mut self,
        point_buffer: &mut dyn PointBufferWriteable,
        count: usize,
    ) -> Result<usize> {
        let num_points_to_read = usize::min(count, self.remaining_points());
        if num_points_to_read == 0 {
            return Ok(0);
        }

        // Read in interleaved chunks, even if the `point_buffer` is not interleaved. `push_points_interleaved` will
        // handle the memory transpose in this case
        let chunk_size = 50_000;
        let point_size = point_buffer.point_layout().size_of_point_entry() as usize;
        let chunk_bytes = point_size * chunk_size;
        let num_chunks = (num_points_to_read + chunk_size - 1) / chunk_size;
        let mut points_chunk: Vec<u8> = vec![0; chunk_bytes];

        let decompression_chunk_size = self.size_of_point_in_file as usize * chunk_size;
        let mut decompression_chunk: Vec<u8> = vec![0; decompression_chunk_size];

        for chunk_index in 0..num_chunks {
            let points_in_chunk =
                std::cmp::min(chunk_size, num_points_to_read - (chunk_index * chunk_size));
            let bytes_in_chunk = points_in_chunk * point_size;

            self.read_chunk_custom_layout(
                &mut points_chunk[..],
                &mut decompression_chunk[..],
                points_in_chunk,
                point_buffer.point_layout(),
            )?;

            point_buffer.push_points_interleaved(&InterleavedPointView::from_raw_slice(
                &points_chunk[0..bytes_in_chunk],
                point_buffer.point_layout().clone(),
            ));
        }

        self.current_point_index += num_points_to_read;

        Ok(num_points_to_read)
    }

    fn read_next_world_space_position(
        &self,
        decompressed_data: &mut Cursor<&mut [u8]>,
    ) -> Result<Vector3<f64>> {
        let local_x = decompressed_data.read_u32::<LittleEndian>()?;
        let local_y = decompressed_data.read_u32::<LittleEndian>()?;
        let local_z = decompressed_data.read_u32::<LittleEndian>()?;
        let global_x = (local_x as f64 * self.point_scales.x) + self.point_offsets.x;
        let global_y = (local_y as f64 * self.point_scales.y) + self.point_offsets.y;
        let global_z = (local_z as f64 * self.point_scales.z) + self.point_offsets.z;
        Ok(Vector3::new(global_x, global_y, global_z))
    }

    fn read_next_bit_attributes(
        &self,
        decompressed_data: &mut Cursor<&mut [u8]>,
        las_format: &Format,
    ) -> Result<BitAttributes> {
        if las_format.is_extended {
            let low_byte = decompressed_data.read_u8()?;
            let high_byte = decompressed_data.read_u8()?;

            Ok(BitAttributes::Extended(BitAttributesExtended {
                return_number: low_byte & 0b1111,
                number_of_returns: (low_byte >> 4) & 0b1111,
                classification_flags: high_byte & 0b1111,
                scanner_channel: (high_byte >> 4) & 0b11,
                scan_direction_flag: (high_byte >> 6) & 0b1,
                edge_of_flight_line: (high_byte >> 7) & 0b1,
            }))
        } else {
            let byte = decompressed_data.read_u8()?;
            Ok(BitAttributes::Regular(BitAttributesRegular {
                return_number: byte & 0b111,
                number_of_returns: (byte >> 3) & 0b111,
                scan_direction_flag: (byte >> 6) & 0b1,
                edge_of_flight_line: (byte >> 7) & 0b1,
            }))
        }
    }

    fn read_next_colors_or_default(
        decompressed_data: &mut Cursor<&mut [u8]>,
        las_format: &Format,
    ) -> Result<Vector3<u16>> {
        if !las_format.has_color {
            return Ok(Default::default());
        }
        let r = decompressed_data.read_u16::<LittleEndian>()?;
        let g = decompressed_data.read_u16::<LittleEndian>()?;
        let b = decompressed_data.read_u16::<LittleEndian>()?;
        Ok(Vector3::new(r, g, b))
    }

    fn read_next_waveform_parameters_or_default(
        decompressed_data: &mut Cursor<&mut [u8]>,
        las_format: &Format,
    ) -> Result<Vector3<f32>> {
        if !las_format.has_waveform {
            return Ok(Default::default());
        }
        let px = decompressed_data.read_f32::<LittleEndian>()?;
        let py = decompressed_data.read_f32::<LittleEndian>()?;
        let pz = decompressed_data.read_f32::<LittleEndian>()?;
        Ok(Vector3::new(px, py, pz))
    }
}

impl<'a, T: Read + Seek + Send + 'a> LASReaderBase for RawLAZReader<'a, T> {
    fn remaining_points(&self) -> usize {
        self.metadata.point_count() - self.current_point_index
    }
}

impl<'a, T: Read + Seek + Send + 'a> PointReader for RawLAZReader<'a, T> {
    fn read(&mut self, count: usize) -> Result<Box<dyn PointBuffer>> {
        let num_points_to_read = usize::min(count, self.remaining_points());
        let mut buffer =
            InterleavedVecPointStorage::with_capacity(num_points_to_read, self.layout.clone());

        self.read_into(&mut buffer, num_points_to_read)?;

        Ok(Box::new(buffer))
    }

    fn read_into(
        &mut self,
        point_buffer: &mut dyn PointBufferWriteable,
        count: usize,
    ) -> Result<usize> {
        if *point_buffer.point_layout() != self.layout {
            self.read_into_custom_layout(point_buffer, count)
        } else {
            self.read_into_default_layout(point_buffer, count)
        }
    }

    fn get_metadata(&self) -> &dyn Metadata {
        &self.metadata
    }

    fn get_default_point_layout(&self) -> &PointLayout {
        &self.layout
    }
}

impl<'a, T: Read + Seek + Send + 'a> SeekToPoint for RawLAZReader<'a, T> {
    fn seek_point(&mut self, position: SeekFrom) -> Result<usize> {
        let new_position = match position {
            SeekFrom::Start(from_start) => from_start as i64,
            SeekFrom::End(from_end) => self.metadata.point_count() as i64 + from_end,
            SeekFrom::Current(from_current) => self.current_point_index as i64 + from_current,
        };
        if new_position < 0 {
            panic!("RawLAZReader::seek_point: It is an error to seek to a point position smaller than zero!");
        }
        let clamped_position =
            std::cmp::min(self.metadata.point_count() as i64, new_position) as usize;

        if self.current_point_index != clamped_position {
            self.reader.seek(clamped_position as u64)?;
            self.current_point_index = clamped_position;
        }

        Ok(self.current_point_index)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::File, io::BufReader};

    use las_rs::point::Format;
    use pasture_core::containers::attributes;
    use pasture_core::layout::PointAttributeDataType;

    use crate::las::{
        compare_to_reference_data, compare_to_reference_data_range, get_test_las_path,
        get_test_laz_path, test_data_bounds, test_data_classifications, test_data_point_count,
        test_data_point_source_ids, test_data_positions, test_data_wavepacket_parameters,
    };

    use super::*;

    // LAS:
    // - Check that metadata is correct (num points etc.)
    // - `read` has to be correct
    //  - it has to return a buffer with the expected format
    //  - it has to return the correct points
    // - `read_into` has to be correct for a buffer with the same layout
    // - `read_into` has to be correct for a buffer with a different layout
    //  - all attributes, but different formats
    //  - some attributes missing
    // - `seek` has to be correct
    //  - it finds the correct position (checked by successive read call)
    //  - it deals correctly with out of bounds, forward, backward search

    macro_rules! test_read_with_format {
        ($name:ident, $format:expr, $reader:ident, $get_test_file:ident) => {
            mod $name {
                use super::*;
                use pasture_core::containers::PerAttributeVecPointStorage;
                use std::path::PathBuf;

                fn get_test_file_path() -> PathBuf {
                    $get_test_file($format)
                }

                #[test]
                fn test_raw_las_reader_metadata() -> Result<()> {
                    let read = BufReader::new(File::open(get_test_file_path())?);
                    let mut reader = $reader::from_read(read)?;

                    assert_eq!(reader.remaining_points(), test_data_point_count());
                    assert_eq!(reader.point_count()?, test_data_point_count());
                    assert_eq!(reader.point_index()?, 0);

                    let layout = reader.get_default_point_layout();
                    let expected_layout =
                        point_layout_from_las_point_format(&Format::new($format)?)?;
                    assert_eq!(expected_layout, *layout);

                    let bounds = reader.get_metadata().bounds();
                    let expected_bounds = test_data_bounds();
                    assert_eq!(Some(expected_bounds), bounds);

                    Ok(())
                }

                #[test]
                fn test_raw_las_reader_read() -> Result<()> {
                    let read = BufReader::new(File::open(get_test_file_path())?);
                    let mut reader = $reader::from_read(read)?;

                    let points = reader.read(10)?;
                    let expected_layout =
                        point_layout_from_las_point_format(&Format::new($format)?)?;
                    assert_eq!(*points.point_layout(), expected_layout);
                    compare_to_reference_data(points.as_ref(), ($format));

                    assert_eq!(10, reader.point_index()?);
                    assert_eq!(0, reader.remaining_points());

                    Ok(())
                }

                #[test]
                fn test_raw_las_reader_read_into_interleaved() -> Result<()> {
                    let read = BufReader::new(File::open(get_test_file_path())?);
                    let mut reader = $reader::from_read(read)?;

                    let layout = point_layout_from_las_point_format(&Format::new($format)?)?;
                    let mut buffer = InterleavedVecPointStorage::new(layout);

                    reader.read_into(&mut buffer, 10)?;
                    compare_to_reference_data(&buffer, $format);

                    assert_eq!(10, reader.point_index()?);
                    assert_eq!(0, reader.remaining_points());

                    Ok(())
                }

                #[test]
                fn test_raw_las_reader_read_into_perattribute() -> Result<()> {
                    let read = BufReader::new(File::open(get_test_file_path())?);
                    let mut reader = $reader::from_read(read)?;

                    let layout = point_layout_from_las_point_format(&Format::new($format)?)?;
                    let mut buffer = PerAttributeVecPointStorage::new(layout);

                    reader.read_into(&mut buffer, 10)?;
                    compare_to_reference_data(&buffer, $format);

                    assert_eq!(10, reader.point_index()?);
                    assert_eq!(0, reader.remaining_points());

                    Ok(())
                }

                #[test]
                fn test_raw_las_reader_read_into_different_attribute_interleaved() -> Result<()> {
                    let read = BufReader::new(File::open(get_test_file_path())?);
                    let mut reader = $reader::from_read(read)?;

                    let format = Format::new($format)?;
                    let layout = PointLayout::from_attributes(&[
                        attributes::POSITION_3D
                            .with_custom_datatype(PointAttributeDataType::Vec3f32),
                        attributes::CLASSIFICATION
                            .with_custom_datatype(PointAttributeDataType::U32),
                        attributes::POINT_SOURCE_ID,
                        attributes::WAVEFORM_PARAMETERS,
                    ]);
                    let mut buffer = InterleavedVecPointStorage::new(layout);

                    reader.read_into(&mut buffer, 10)?;

                    let positions = attributes::<Vector3<f32>>(
                        &buffer,
                        &attributes::POSITION_3D
                            .with_custom_datatype(PointAttributeDataType::Vec3f32),
                    )
                    .collect::<Vec<_>>();
                    let expected_positions = test_data_positions()
                        .into_iter()
                        .map(|p| Vector3::new(p.x as f32, p.y as f32, p.z as f32))
                        .collect::<Vec<_>>();
                    assert_eq!(expected_positions, positions, "Positions do not match");

                    let classifications = attributes::<u32>(
                        &buffer,
                        &attributes::CLASSIFICATION
                            .with_custom_datatype(PointAttributeDataType::U32),
                    )
                    .collect::<Vec<_>>();
                    let expected_classifications = test_data_classifications()
                        .into_iter()
                        .map(|c| c as u32)
                        .collect::<Vec<_>>();
                    assert_eq!(
                        expected_classifications, classifications,
                        "Classifications do not match"
                    );

                    let point_source_ids = attributes::<u16>(&buffer, &attributes::POINT_SOURCE_ID)
                        .collect::<Vec<_>>();
                    let expected_point_source_ids = test_data_point_source_ids();
                    assert_eq!(
                        expected_point_source_ids, point_source_ids,
                        "Point source IDs do not match"
                    );

                    let waveform_params =
                        attributes::<Vector3<f32>>(&buffer, &attributes::WAVEFORM_PARAMETERS)
                            .collect::<Vec<_>>();
                    let expected_waveform_params = if format.has_waveform {
                        test_data_wavepacket_parameters()
                    } else {
                        (0..10)
                            .map(|_| -> Vector3<f32> { Default::default() })
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(
                        expected_waveform_params, waveform_params,
                        "Wavepacket parameters do not match"
                    );

                    assert_eq!(10, reader.point_index()?);
                    assert_eq!(0, reader.remaining_points());

                    Ok(())
                }

                #[test]
                fn test_raw_las_reader_seek() -> Result<()> {
                    let read = BufReader::new(File::open(get_test_file_path())?);
                    let mut reader = $reader::from_read(read)?;

                    let seek_index: usize = 5;
                    let new_pos = reader.seek_point(SeekFrom::Current(seek_index as i64))?;
                    assert_eq!(seek_index, new_pos);

                    let points = reader.read((10 - seek_index) as usize)?;
                    assert_eq!(10 - seek_index, points.len());

                    compare_to_reference_data_range(points.as_ref(), $format, seek_index..10);

                    Ok(())
                }

                #[test]
                fn test_raw_las_reader_seek_out_of_bounds() -> Result<()> {
                    let read = BufReader::new(File::open(get_test_file_path())?);
                    let mut reader = $reader::from_read(read)?;

                    let seek_index: usize = 23;
                    let new_pos = reader.seek_point(SeekFrom::Current(seek_index as i64))?;
                    assert_eq!(10, new_pos);

                    let points = reader.read(10)?;
                    assert_eq!(0, points.len());

                    Ok(())
                }
            }
        };
    }

    test_read_with_format!(las_format_0, 0, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_1, 1, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_2, 2, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_3, 3, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_4, 4, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_5, 5, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_6, 6, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_7, 7, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_8, 8, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_9, 9, RawLASReader, get_test_las_path);
    test_read_with_format!(las_format_10, 10, RawLASReader, get_test_las_path);

    test_read_with_format!(laz_format_0, 0, RawLAZReader, get_test_laz_path);
    test_read_with_format!(laz_format_1, 1, RawLAZReader, get_test_laz_path);
    test_read_with_format!(laz_format_2, 2, RawLAZReader, get_test_laz_path);
    test_read_with_format!(laz_format_3, 3, RawLAZReader, get_test_laz_path);
    // Formats 4,5,9,10 have wave packet data, which is currently unsupported by laz-rs
    // Format 6,7,8 seem to be unsupported by LASzip and give weird results with laz-rs (e.g. seek does not work correctly)
    // test_read_with_format!(laz_format_4, 4, RawLAZReader);
    // test_read_with_format!(laz_format_5, 5, RawLAZReader);
    // test_read_with_format!(laz_format_6, 6, RawLAZReader, get_test_laz_path);
    // test_read_with_format!(laz_format_7, 7, RawLAZReader, get_test_laz_path);
    // test_read_with_format!(laz_format_8, 8, RawLAZReader, get_test_laz_path);
    // test_read_with_format!(laz_format_9, 9, RawLAZReader);
    // test_read_with_format!(laz_format_10, 10, RawLAZReader);

    //######### TODO ###########
    // We have tests now for various formats and various conversions. We should extend them for a wider range, maybe even
    // fuzz-test (though this is more effort to setup...)
    // Also include comparisons for the additional attributes in the '_read_into_different_attribute_...' tests
}