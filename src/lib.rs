pub mod adjustment;
pub mod correlation;
mod external_sort;

pub mod experiment {
    use crate::correlation::{get_correlation_method, CorrelationMethod};
    use crate::{
        adjustment::{get_adjustment_method, AdjustmentMethod},
        external_sort::{Sortable, ExternalSorter}
    };
    use csv::ReaderBuilder;
    use pyo3::prelude::*;
    use pyo3::wrap_pyfunction;
    use std::{fmt::Debug, io::{Read, Write}};
    use itertools::iproduct;
    use serde_derive::{Deserialize, Serialize};
    use std::cmp::Ordering;

    type VecOfResults = Vec<CorResult>;
    type TupleExpressionValues = (String, Vec<f64>);
    pub type Batch = Vec<TupleExpressionValues>;
    type LazyMatrix = Box<dyn Iterator<Item = TupleExpressionValues>>;

    #[pyclass]
    #[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
    pub struct CorResult {
        #[pyo3(get, set)]
        gene: String,
        #[pyo3(get, set)]
        gem: String,
        #[pyo3(get, set)]
        correlation: f64,
        #[pyo3(get, set)]
        p_value: f64,
        #[pyo3(get, set)]
        adjusted_p_value: Option<f64>,
    }

    impl std::fmt::Display for CorResult {
        // This trait requires `fmt` with this exact signature.
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            // Write strictly the first element into the supplied output
            // stream: `f`. Returns `fmt::Result` which indicates whether the
            // operation succeeded or failed. Note that `write!` uses syntax which
            // is very similar to `println!`.
            write!(
                f,
                "Gene -> {} | GEM -> {}\n\tCor -> {}\n\tP-value -> {:+e}\n\tAdjusted p-value -> {:+e}",
                self.gene, self.gem, &self.correlation, &self.p_value, &self.adjusted_p_value.unwrap()
            )
        }
    }

    impl Eq for CorResult {}

    impl Ord for CorResult {
        // Sorts in descending order
        fn cmp(&self, other: &Self) -> Ordering {
            self.partial_cmp(&other).unwrap()
        }
    }

    impl PartialOrd for CorResult {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            self.p_value.partial_cmp(&other.p_value)
        }
    }

    impl Sortable for CorResult {
        fn encode<W: Write>(&self, writer: &mut W) {
            let serialized = bincode::serialize(self).unwrap();
            writer.write_all(&serialized[..]).unwrap();
        }

        fn decode<R: Read>(reader: &mut R) -> Option<Self> {
            bincode::deserialize_from(reader).ok()
        }
    }

    pub trait Computation {
        fn all_vs_all(
            &self,
            m1: LazyMatrix,
            len_m1: u64,
            m3: LazyMatrix,
            len_m3: u64,
            number_of_columns: usize,
            correlation_method: CorrelationMethod,
            correlation_threhold: f64,
            sort_buf_size: u64,
            adjustment_method: AdjustmentMethod,
        ) -> VecOfResults {
            let total_number_of_elements: u64 = len_m1 * len_m3;

            // We need a collected object for right-side of the iproduct macro. In this
            // case it gets the smaller one to collect
            let (lazy_m, collected_m) = if len_m1 > len_m3 { (m1, m3) } else { (m3, m1) };

            let collected_m = collected_m.collect::<Vec<TupleExpressionValues>>();

            let correlation_struct = get_correlation_method(correlation_method, number_of_columns);
            let correlations_and_p_values =
                iproduct!(lazy_m, collected_m).map(|(tuple1, tuple3)| {
                    // Gene and GEM
                    let gene = tuple1.0;
                    let gem = tuple3.0;

                    let (correlation, p_value) =
                        correlation_struct.correlate(tuple1.1.as_slice(), tuple3.1.as_slice());

                    CorResult {
                        gene,
                        gem,
                        correlation,
                        p_value,
                        adjusted_p_value: None,
                    }
                });

            // Sorting
            let sorted: Box<dyn Iterator<Item = CorResult>> = match adjustment_method {
                AdjustmentMethod::Bonferroni => Box::new(correlations_and_p_values),
                _ => {
                    // Benjamini-Hochberg and Benajmini-Yekutieli needs sort by p-value to
                    // make the adjustment
                    let mut sorter = ExternalSorter::new(sort_buf_size as usize);
                    Box::new(sorter.sort(correlations_and_p_values).unwrap())
                }
            };

            // Ranking
            let ranked = sorted.enumerate();

            // Filtering
            let filtered = ranked.filter(|(_, cor_and_p_value)| {
                cor_and_p_value.correlation.abs() >= correlation_threhold
            });

            // Adjustment
            let mut adjustment_struct =
                get_adjustment_method(adjustment_method, total_number_of_elements as f64);
            let adjusted = filtered.map(|(rank, mut cor_and_p_value)| {
                let p_value = cor_and_p_value.p_value;
                let q_value = adjustment_struct.adjust(p_value, rank);

                cor_and_p_value.adjusted_p_value = Some(q_value);
                cor_and_p_value
            });

            adjusted.collect::<VecOfResults>()
        }

        fn get_df(&self, path: &str) -> LazyMatrix {
            // Build the CSV reader and iterate over each record.
            let rdr = ReaderBuilder::new()
                .delimiter(b'\t')
                .from_path(path)
                .unwrap();

            let dataframe_parsed =
                rdr.into_records()
                    .enumerate()
                    .map(|(row_idx, record_result)| {
                        let record = record_result.unwrap();
                        let mut it = record.into_iter();
                        let gene_or_gem = it.next().unwrap().to_string();
                        let values = it
                            .enumerate()
                            .map(|(column_idx, cell)| {
                                // It's added 2 as we're not considering headers row or index column
                                cell.parse::<f64>().expect(&format!(
                                    "Row {} column {} has an invalid value",
                                    row_idx + 2,
                                    column_idx + 2
                                ))
                            })
                            .collect::<Vec<f64>>();

                        (gene_or_gem, values)
                    });

            Box::new(dataframe_parsed)
        }

        fn get_both_df_and_shape(
            &self,
            df1_path: &str,
            df2_path: &str,
        ) -> (LazyMatrix, u64, LazyMatrix, u64, usize) {
            let m1 = self.get_df(df1_path);
            let mut m1_aux = self.get_df(df1_path);
            let number_of_columns = m1_aux.next().unwrap().1.len();
            let len_m1 = m1_aux.count() + 1; // Plus discarded element by next()

            let m3 = self.get_df(df2_path);
            let m3_aux = self.get_df(df2_path);
            let len_m3 = m3_aux.count();

            (m1, len_m1 as u64, m3, len_m3 as u64, number_of_columns)
        }

        fn compute(
            &self,
            correlation_method: CorrelationMethod,
            correlation_threhold: f64,
            sort_buf_size: u64,
            adjustment_method: AdjustmentMethod,
        ) -> VecOfResults;
    }

    pub struct ExperimentFromFiles {
        file_1_path: String,
        file_2_path: String,
    }

    impl Computation for ExperimentFromFiles {
        fn compute(
            &self,
            correlation_method: CorrelationMethod,
            correlation_threhold: f64,
            sort_buf_size: u64,
            adjustment_method: AdjustmentMethod,
        ) -> VecOfResults {
            let (m1, len_m1, m3, len_m3, number_of_columns) =
                self.get_both_df_and_shape(self.file_1_path.as_str(), self.file_2_path.as_str());

            self.all_vs_all(
                m1,
                len_m1,
                m3,
                len_m3,
                number_of_columns,
                correlation_method,
                correlation_threhold,
                sort_buf_size,
                adjustment_method,
            )
        }
    }

    pub fn new_from_files(file_1_path: String, file_2_path: String) -> ExperimentFromFiles {
        ExperimentFromFiles {
            file_1_path,
            file_2_path,
        }
    }

    /// A Python module implemented in Rust.
    #[pymodule]
    fn ggca(_py: Python, m: &PyModule) -> PyResult<()> {
        m.add_function(wrap_pyfunction!(correlate, m)?)?;

        Ok(())
    }

    #[pyfunction]
    pub fn correlate(
        py: Python,
        file_1_path: String,
        file_2_path: String,
        correlation_method: i32,
        correlation_threhold: f64,
        sort_buf_size: u64,
        adjustment_method: i32,
    ) -> PyResult<VecOfResults> {
        py.allow_threads(|| {
            let experiment = new_from_files(file_1_path, file_2_path);
            let correlation_method = match correlation_method {
                1 => CorrelationMethod::Spearman,
                2 => CorrelationMethod::Kendall,
                _ => CorrelationMethod::Pearson,
            };

            let adjustment_method = match adjustment_method {
                1 => AdjustmentMethod::BenjaminiHochberg,
                2 => AdjustmentMethod::BenjaminiYekutieli,
                _ => AdjustmentMethod::Bonferroni,
            };

            let result = experiment.compute(
                correlation_method,
                correlation_threhold,
                sort_buf_size,
                adjustment_method,
            );
            Ok(result)
        })
    }
}