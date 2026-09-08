wit_bindgen::generate!({world:"tool",path:"../../wit",generate_all});
mod miro;
use miro::Miro;
use serde_json::{json,Value};
struct Component;
impl Guest for Component {
 fn describe()->ToolManifest { ToolManifest {
  name:"miro-item-create-bulk".into(),
  description:"Transactionally create 1-20 mixed item types with one POST /boards/{id}/items/bulk request.".into(),
  args_schema_json:json!({
   "type":"object",
   "properties":{
    "board_id":{"type":"string"},
    "items":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"object","description":"A Miro v2 ItemCreate object. Include its required type property and type-specific data/style/position/geometry fields."}}
   },
   "required":["board_id","items"],
   "additionalProperties":false
  }).to_string(),
  capabilities:vec!["http".into(),"boards:write".into(),"group:miro".into()]
 }}
 fn invoke(_:String,a:String,c:String)->Result<String,String> {
  let a=miro::args(&a)?;
  let id=miro::req(&a,"board_id")?;
  let xs=a.get("items").and_then(Value::as_array).ok_or("items must be an array")?;
  if xs.is_empty()||xs.len()>20{return Err("items must contain 1-20 entries".into())}
  for (i,item) in xs.iter().enumerate(){
   if !item.is_object(){return Err(format!("items[{i}] must be an object"))}
   if item.get("type").and_then(Value::as_str).map(str::trim).filter(|s|!s.is_empty()).is_none(){return Err(format!("items[{i}].type must be a non-empty string"))}
  }
  let path=format!("/boards/{}/items/bulk",miro::enc(id));
  let body=Value::Array(xs.clone());
  Ok(miro::normalized(&Miro::from_config(&c)?.request("POST",&path,&[],Some(&body),false)?))
 }
}
export!(Component);
