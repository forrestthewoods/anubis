// Smallest useful CPython extension: hello.add(a, b) and hello.greet(name).
#define PY_SSIZE_T_CLEAN
// Windows debug builds: pyconfig.h turns _DEBUG into Py_DEBUG, a different
// ABI that needs a debug build of Python (python312_d.lib). Hide _DEBUG from
// Python.h so a debug extension runs on the release Python (pybind11 does
// the same for its users).
#if defined(_MSC_VER) && defined(_DEBUG)
#undef _DEBUG
#include <Python.h>
#define _DEBUG 1
#else
#include <Python.h>
#endif

static PyObject* hello_add(PyObject* self, PyObject* args) {
    long a, b;
    if (!PyArg_ParseTuple(args, "ll", &a, &b)) {
        return NULL;
    }
    return PyLong_FromLong(a + b);
}

static PyObject* hello_greet(PyObject* self, PyObject* args) {
    const char* name;
    if (!PyArg_ParseTuple(args, "s", &name)) {
        return NULL;
    }
    return PyUnicode_FromFormat("hello, %s", name);
}

static PyMethodDef hello_methods[] = {
    {"add", hello_add, METH_VARARGS, "Add two integers."},
    {"greet", hello_greet, METH_VARARGS, "Greet someone."},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef hello_module = {
    PyModuleDef_HEAD_INIT, "hello", "Built by Anubis.", -1, hello_methods,
};

PyMODINIT_FUNC PyInit_hello(void) {
    return PyModule_Create(&hello_module);
}
