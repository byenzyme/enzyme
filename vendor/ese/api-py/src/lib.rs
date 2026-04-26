use numpy::PyArrayMethods;
use pyo3::prelude::*;

#[pyfunction]
fn encode<'py>(py: Python<'py>, texts: Py<PyAny>) -> Bound<'py, numpy::PyArray2<f32>> {
    let texts: Vec<String> = if let Ok(s) = texts.extract::<String>(py) {
        vec![s]
    } else {
        texts
            .extract::<Vec<String>>(py)
            .expect("expected str or list of str")
    };
    let vecs = ese_core::encode(texts);
    let rows = vecs.len();
    let cols = ese_core::DIMENSIONS;
    let flat: Vec<f32> = vecs.into_iter().flatten().collect();
    numpy::PyArray1::from_vec(py, flat)
        .reshape([rows, cols])
        .unwrap()
}

#[pyfunction]
fn dimensions() -> usize {
    ese_core::DIMENSIONS
}

#[pymodule]
fn ese(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(encode, m)?)?;
    m.add_function(wrap_pyfunction!(dimensions, m)?)?;
    m.add("DIMENSIONS", ese_core::DIMENSIONS)?;
    Ok(())
}
