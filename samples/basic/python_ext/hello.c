// Smallest useful CPython extension: hello.add(a, b) and hello.greet(name).
#define PY_SSIZE_T_CLEAN
#include <Python.h>

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
