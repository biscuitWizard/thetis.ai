wit_bindgen::generate!({world:"tool",path:"../../wit",generate_all});
mod miro; use miro::Miro; use serde_json::json;
struct Component;
impl Guest for Component{
 fn describe()->ToolManifest{ToolManifest{name:"miro-board-list".into(),description:"List Miro boards with bounded offset pagination.".into(),args_schema_json:json!({"type":"object","properties":{"query":{"type":"string"},"team_id":{"type":"string"},"project_id":{"type":"string"},"owner":{"type":"string"},"sort":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"additionalProperties":false}).to_string(),capabilities:vec!["http".into(),"read-only".into(),"group:miro".into()]}}
 fn invoke(_:String,a:String,c:String)->Result<String,String>{let a=miro::args(&a)?;let cli=Miro::from_config(&c)?;let (r,n)=cli.paginate("/boards",miro::q(&a,&["query","team_id","project_id","owner","sort","offset"]),miro::limit(&a),"offset")?;Ok(miro::list_output(r,n))}
}
export!(Component);
