pub mod emit_py;

use coatl_core::{format_errs, transpile_to_py_ast, TranspileOptions};
use pyo3::prelude::*;

#[pyfunction(signature=(src, mode="module", filename="<string>"))]
fn transpile(src: &str, mode: &str, filename: &str) -> PyResult<PyObject> {
    let options = match mode {
        "module" => TranspileOptions::module(),
        "prelude" => TranspileOptions::prelude(),
        "interactive" => TranspileOptions::interactive(),
        "script" => TranspileOptions::script(),
        _ => {
            return Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(
                "Invalid mode. Use 'module' or 'prelude' or 'interactive' or 'script'.",
            ))
        }
    };

    let py_ast = transpile_to_py_ast(src, options).map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyException, _>(format_errs(&e, filename, src))
    })?;

    let py_ast_obj = emit_py::emit_py(&py_ast, src).map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyException, _>(format!("Emission error: {}", e.message))
    })?;

    Ok(py_ast_obj)
}

#[pymodule(name = "_rs")]
fn py_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(transpile, m)?)?;
    Ok(())
}
