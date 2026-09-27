"""Single QQ message runtime and JSON Schema contract for ``send_message``."""

from __future__ import annotations

from functools import cache
from typing import Annotated, ClassVar, Literal

from pydantic import BaseModel, ConfigDict, Field, create_model, model_validator
from pydantic_core import PydanticCustomError


class _ContractModel(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True, strict=True)


class TextData(_ContractModel):
    text: str


class MemberData(_ContractModel):
    member: Annotated[
        int,
        Field(strict=True, gt=0, description="成员名字后的 ⟦N⟧ 编号"),
    ]


class LineData(_ContractModel):
    line: Annotated[
        int,
        Field(strict=True, gt=0, description="发言行首的 #N 编号"),
    ]


class FaceData(_ContractModel):
    id: Annotated[int, Field(strict=True, ge=0)]


class EmptyData(_ContractModel):
    pass


class TextInput(_ContractModel):
    type: Literal["text"]
    data: TextData


class AtInput(_ContractModel):
    type: Literal["at"]
    data: MemberData


class ReplyInput(_ContractModel):
    type: Literal["reply"]
    data: LineData


class FaceInput(_ContractModel):
    type: Literal["face"]
    data: FaceData


class DiceInput(_ContractModel):
    type: Literal["dice"]
    data: EmptyData


class RpsInput(_ContractModel):
    type: Literal["rps"]
    data: EmptyData


class ContactMemberInput(_ContractModel):
    type: Literal["contact_member"]
    data: MemberData


class ContactGroupInput(_ContractModel):
    type: Literal["contact_group"]
    data: EmptyData


SendSegmentInput = Annotated[
    TextInput
    | AtInput
    | ReplyInput
    | FaceInput
    | DiceInput
    | RpsInput
    | ContactMemberInput
    | ContactGroupInput,
    Field(discriminator="type"),
]

_EXCLUSIVE_INPUTS = (DiceInput, RpsInput, ContactMemberInput, ContactGroupInput)


class SendMessageInput(_ContractModel):
    content: list[SendSegmentInput] = Field(
        min_length=1,
        max_length=32,
        description="按发送顺序排列的 QQ 消息段；@ 可插在任意位置。",
    )

    @model_validator(mode="after")
    def _validate_shape(self) -> SendMessageInput:
        if sum(isinstance(item, ReplyInput) for item in self.content) > 1:
            raise ValueError("a message may contain at most one reply segment")
        if sum(isinstance(item, AtInput) for item in self.content) > 5:
            raise ValueError("a message may contain at most five at segments")
        if (
            any(isinstance(item, _EXCLUSIVE_INPUTS) for item in self.content)
            and len(self.content) != 1
        ):
            raise ValueError("random and contact segments must occupy the whole message")
        visible = any(
            not isinstance(item, ReplyInput)
            and (not isinstance(item, TextInput) or bool(item.data.text.strip()))
            for item in self.content
        )
        if not visible:
            raise PydanticCustomError("send_empty", "message has no visible content")
        return self


class _SendArguments(_ContractModel):
    max_text_chars: ClassVar[int]
    content: list[SendSegmentInput] = Field(min_length=1, max_length=32)

    @model_validator(mode="after")
    def _validate_message(self) -> _SendArguments:
        SendMessageInput(content=self.content)
        total = sum(len(item.data.text) for item in self.content if isinstance(item, TextInput))
        if total > self.max_text_chars:
            raise ValueError("message text exceeds the configured character limit")
        return self


@cache
def send_arguments_model(max_text_chars: int) -> type[_SendArguments]:
    """Use one identical configured model for validation and the tool schema."""

    if max_text_chars < 1:
        raise ValueError("send text limit must be positive")
    model = create_model("SendMessageArguments", __base__=_SendArguments)
    model.max_text_chars = max_text_chars
    return model
