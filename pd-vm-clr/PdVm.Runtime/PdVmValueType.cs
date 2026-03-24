namespace PdVm.Runtime;

public enum PdVmValueType : byte
{
    Unknown = 0,
    Null = 1,
    Int = 2,
    Float = 3,
    Bool = 4,
    String = 5,
    Bytes = 6,
    Array = 7,
    Map = 8,
}
