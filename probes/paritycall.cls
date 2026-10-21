Class SQLUser.ParityCall Extends %RegisteredObject
{

ClassMethod Ping() As %Integer [ SqlProc, SqlName = ParityCall_Ping ]
{
    Quit 1
}

ClassMethod Echo(Val As %String) As %String [ SqlProc, SqlName = ParityCall_Echo ]
{
    Quit Val
}

}
