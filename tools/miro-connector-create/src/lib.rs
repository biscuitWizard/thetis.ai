wit_bindgen::generate!({world:"tool",path:"../../wit",generate_all});
mod miro; use miro::Miro; use serde_json::{json,Value};
struct Component;
impl Guest for Component{
 fn describe()->ToolManifest{ToolManifest{name:"miro-connector-create".into(),description:"Create a Miro connector.".into(),args_schema_json:json!({"type":"object","properties":{"board_id":{"type":"string"},"body":{"type":"object"}},"required":["board_id","body"],"additionalProperties":false}).to_string(),capabilities:vec!["http".into(),"group:miro".into()]}}
 fn invoke(_:String,a:String,c:String)->Result<String,String>{
  let a=miro::args(&a)?; let p=format!("/boards/{}/connectors",miro::enc(miro::req(&a,"board_id")?)); let b=miro::obj(&a,"body")?;
  let endpoint_id=|name:&str|->Result<&str,String>{b.get(name).and_then(Value::as_object).ok_or_else(||format!("body.{name} must be an object"))?.get("id").and_then(Value::as_str).map(str::trim).filter(|s|!s.is_empty()).ok_or_else(||format!("body.{name}.id must be a non-empty string"))};
  let start=endpoint_id("startItem")?; let end=endpoint_id("endItem")?; if start==end{return Err("body.startItem.id and body.endItem.id must be distinct".into())}
  if let Some(shape)=b.get("shape"){let shape=shape.as_str().ok_or("body.shape must be a string")?;if !["straight","elbowed","curved"].contains(&shape){return Err("body.shape must be straight, elbowed, or curved".into())}}
  Ok(miro::normalized(&Miro::from_config(&c)?.request("POST",&p,&[],Some(&b),false)?))
 }
}
export!(Component);
