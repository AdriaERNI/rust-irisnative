Class User.ParityCall Extends %RegisteredObject
{

ClassMethod Ping() As %Integer [ SqlProc ]
{
    Quit 1
}

ClassMethod Echo(Val As %String) As %String [ SqlProc ]
{
    Quit Val
}

}
